//! Decoder for omp RPC stdout lines, including protocol-v2 `rpc_chunk` reassembly.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid rpc_chunk: {0}")]
    Chunk(String),
    #[error("frame must be a JSON object")]
    NotObject,
}

/// Upper bound on `rpc_chunk.count`, so a corrupt header cannot announce an endless sequence.
const MAX_CHUNKS: u64 = 4096;

struct Pending {
    chunk_id: String,
    count: u64,
    next: u64,
    byte_length: usize,
    buf: Vec<u8>,
}

pub struct FrameDecoder {
    max_bytes: usize,
    pending: Option<Pending>,
}

impl FrameDecoder {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes, pending: None }
    }

    /// Feeds one stdout line. Returns `Some(frame)` once a complete logical frame is available.
    ///
    /// An `Err` means the stream is corrupt: the caller must not trust anything it reads afterwards.
    pub fn push_line(&mut self, line: &str) -> Result<Option<Value>, FrameError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(line)?;
        if !value.is_object() {
            self.pending = None;
            return Err(FrameError::NotObject);
        }
        if value["type"] != "rpc_chunk" {
            if self.pending.take().is_some() {
                return Err(FrameError::Chunk("chunk sequence interrupted by another frame".into()));
            }
            return Ok(Some(value));
        }
        match self.push_chunk(&value) {
            Ok(frame) => Ok(frame),
            Err(e) => {
                self.pending = None;
                Err(e)
            }
        }
    }

    fn push_chunk(&mut self, v: &Value) -> Result<Option<Value>, FrameError> {
        let bad = |msg: &str| FrameError::Chunk(msg.to_string());
        let chunk_id = v["chunkId"].as_str().ok_or_else(|| bad("missing chunkId"))?;
        let index = v["index"].as_u64().ok_or_else(|| bad("missing index"))?;
        let count = v["count"].as_u64().ok_or_else(|| bad("missing count"))?;
        let byte_length = v["byteLength"].as_u64().ok_or_else(|| bad("missing byteLength"))?;
        let byte_length = usize::try_from(byte_length).map_err(|_| bad("byteLength too large"))?;
        let data = v["data"].as_str().ok_or_else(|| bad("missing data"))?;
        if data.is_empty() {
            return Err(bad("empty data"));
        }

        if index == 0 {
            if self.pending.is_some() {
                return Err(bad("new chunk sequence started before the previous one finished"));
            }
            if count == 0 || count > MAX_CHUNKS || byte_length > self.max_bytes {
                return Err(bad("invalid count or frame too large"));
            }
            self.pending = Some(Pending {
                chunk_id: chunk_id.to_string(),
                count,
                next: 0,
                byte_length,
                buf: Vec::with_capacity(byte_length),
            });
        }
        let p = self.pending.as_mut().ok_or_else(|| bad("chunk without sequence start"))?;
        if p.chunk_id != chunk_id || p.next != index || p.count != count || p.byte_length != byte_length {
            return Err(bad("chunk out of sequence"));
        }
        let bytes = STANDARD.decode(data).map_err(|e| FrameError::Chunk(e.to_string()))?;
        p.buf.extend_from_slice(&bytes);
        if p.buf.len() > p.byte_length {
            return Err(bad("chunk data exceeds byteLength"));
        }
        p.next += 1;
        if p.next < p.count {
            return Ok(None);
        }
        let p = self.pending.take().expect("pending checked above");
        if p.buf.len() != p.byte_length {
            return Err(bad("reassembled length mismatch"));
        }
        let text = String::from_utf8(p.buf).map_err(|_| bad("reassembled frame is not UTF-8"))?;
        let frame: Value = serde_json::from_str(&text)?;
        if !frame.is_object() {
            return Err(FrameError::NotObject);
        }
        if frame["type"] == "rpc_chunk" {
            return Err(bad("reassembled frame is itself an rpc_chunk"));
        }
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunks(id: &str, payload: &Value, parts: usize) -> Vec<String> {
        let bytes = serde_json::to_vec(payload).unwrap();
        let size = bytes.len().div_ceil(parts);
        bytes
            .chunks(size)
            .enumerate()
            .map(|(i, c)| {
                json!({
                    "type": "rpc_chunk", "chunkId": id, "index": i, "count": parts,
                    "byteLength": bytes.len(), "data": STANDARD.encode(c)
                })
                .to_string()
            })
            .collect()
    }

    #[test]
    fn passes_plain_frames_through() {
        let mut d = FrameDecoder::new(1024);
        assert_eq!(d.push_line("").unwrap(), None);
        assert_eq!(d.push_line(r#"{"type":"agent_start"}"#).unwrap(), Some(json!({"type":"agent_start"})));
    }

    #[test]
    fn reassembles_chunked_frames() {
        let payload = json!({"type": "response", "data": "x".repeat(100)});
        let mut d = FrameDecoder::new(1 << 20);
        let lines = chunks("c1", &payload, 3);
        assert_eq!(d.push_line(&lines[0]).unwrap(), None);
        assert_eq!(d.push_line(&lines[1]).unwrap(), None);
        assert_eq!(d.push_line(&lines[2]).unwrap(), Some(payload));
    }

    #[test]
    fn rejects_interleaved_and_out_of_order_chunks() {
        let payload = json!({"type": "x", "data": "y".repeat(50)});
        let lines = chunks("c1", &payload, 2);

        let mut d = FrameDecoder::new(1 << 20);
        d.push_line(&lines[0]).unwrap();
        assert!(d.push_line(r#"{"type":"agent_start"}"#).is_err());
        // decoder recovers afterwards
        assert!(d.push_line(r#"{"type":"agent_end"}"#).unwrap().is_some());

        let mut d = FrameDecoder::new(1 << 20);
        assert!(d.push_line(&lines[1]).is_err());
    }

    fn chunk(index: u64, count: u64, byte_length: u64, data: &str) -> String {
        json!({"type": "rpc_chunk", "chunkId": "c", "index": index, "count": count,
               "byteLength": byte_length, "data": data})
        .to_string()
    }

    #[test]
    fn rejects_plain_frames_that_are_not_objects() {
        for line in ["[1]", "\"agent_start\"", "3", "null"] {
            let mut d = FrameDecoder::new(1024);
            assert!(matches!(d.push_line(line), Err(FrameError::NotObject)), "{line}");
        }
    }

    #[test]
    fn rejects_reassembled_frames_that_are_not_objects_or_are_chunks() {
        let mut d = FrameDecoder::new(1 << 20);
        let lines = chunks("c1", &json!([1, 2, 3]), 2);
        d.push_line(&lines[0]).unwrap();
        assert!(matches!(d.push_line(&lines[1]), Err(FrameError::NotObject)));

        let mut d = FrameDecoder::new(1 << 20);
        let nested =
            json!({"type": "rpc_chunk", "chunkId": "x", "index": 0, "count": 1, "byteLength": 2, "data": "e30="});
        let lines = chunks("c2", &nested, 2);
        d.push_line(&lines[0]).unwrap();
        assert!(matches!(d.push_line(&lines[1]), Err(FrameError::Chunk(_))));
    }

    #[test]
    fn rejects_too_many_chunks_and_empty_data() {
        let mut d = FrameDecoder::new(1 << 20);
        assert!(d.push_line(&chunk(0, 4097, 10, "e30=")).is_err());
        let mut d = FrameDecoder::new(1 << 20);
        assert!(d.push_line(&chunk(0, 4096, 10, "e30=")).is_ok());

        let mut d = FrameDecoder::new(1 << 20);
        assert!(d.push_line(&chunk(0, 2, 10, "")).is_err());
    }

    #[test]
    fn rejects_oversized_frames() {
        let payload = json!({"data": "z".repeat(100)});
        let mut d = FrameDecoder::new(10);
        assert!(d.push_line(&chunks("c", &payload, 2)[0]).is_err());
    }
}
