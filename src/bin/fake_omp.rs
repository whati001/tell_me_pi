//! Test double for `omp --mode rpc`. Speaks the same JSONL protocol with scripted behaviour
//! selected by keywords in the prompt text:
//!
//! - default: thinking + `echo[<user message count>]: <message>` + usage
//! - `USE_TOOL`: calls the `repo` host tool (`{"action":"list"}`) and echoes its result
//! - `SLOW`: streams `partial` and waits until `abort` / `abort_and_prompt`
//! - `CRASH`: exits with status 3 after acknowledging the prompt
//! - `EXT_UI`: sends a confirm dialog and echoes whether it was cancelled
//! - `FAIL_LATE`: acknowledges the prompt, then fails it with a second response (like a missing login)
//! - `BIG`: sends a 5000 character text delta as `rpc_chunk` frames (v2 only)
//! - `CORRUPT`: sends the first `rpc_chunk` of a two-chunk sequence, then carries on normally (an
//!   `agent_start` frame interrupts the sequence, which corrupts the stream)
//!
//! `get_state` reports the `--tools` list plus the registered host tools as `dumpTools`; with
//! `--model fake/leaky` it also reports `bash`.
//!
//! Every invocation appends its argv as a JSON line to `<session-dir>/../args.log`.

use std::{
    io::{BufRead, Write},
    path::PathBuf,
    thread,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

struct Fake {
    session_dir: PathBuf,
    session_file: PathBuf,
    entries: Vec<(String, String)>,
    next_entry: u64,
    v2: bool,
    tools: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let session_dir = PathBuf::from(arg("--session-dir").expect("--session-dir required"));
    std::fs::create_dir_all(&session_dir).unwrap();
    let log = session_dir.parent().unwrap().join("args.log");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(f, "{}", json!(args)).unwrap();

    let mut tools: Vec<String> =
        arg("--tools").unwrap_or_default().split(',').filter(|t| !t.is_empty()).map(String::from).collect();
    if arg("--model").as_deref() == Some("fake/leaky") {
        tools.push("bash".into());
    }
    let mut fake =
        Fake { session_dir, session_file: PathBuf::new(), entries: Vec::new(), next_entry: 1, v2: false, tools };
    match arg("--resume") {
        Some(path) => {
            fake.session_file = PathBuf::from(&path);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "[]".into());
            let saved: Vec<(String, String)> = serde_json::from_str(&text).unwrap();
            fake.next_entry = saved.len() as u64 + 100;
            fake.entries = saved;
        }
        None => fake.new_file(),
    }

    out(&json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2],
                "maxFrameBytes": 1048576, "maxReassembledFrameBytes": 67108864}));

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let cmd: Value = serde_json::from_str(&line).unwrap();
        fake.handle(&cmd, &mut lines);
    }
}

fn out(v: &Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{v}").unwrap();
    stdout.flush().unwrap();
}

fn ok(cmd: &Value, data: Value) {
    out(&json!({"id": cmd["id"], "type": "response", "command": cmd["type"], "success": true, "data": data}));
}

fn text_delta(text: &str) -> Value {
    json!({"type": "message_update",
           "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": text},
           "message": {"role": "assistant", "content": []}})
}

type Lines<'a> = std::io::Lines<std::io::StdinLock<'a>>;

impl Fake {
    fn new_file(&mut self) {
        let n = std::fs::read_dir(&self.session_dir).unwrap().count();
        self.session_file = self.session_dir.join(format!("session-{n}.json"));
        self.save();
    }

    fn save(&self) {
        std::fs::write(&self.session_file, serde_json::to_string(&self.entries).unwrap()).unwrap();
    }

    fn handle(&mut self, cmd: &Value, lines: &mut Lines) {
        match cmd["type"].as_str().unwrap() {
            "negotiate_protocol" => {
                self.v2 = true;
                ok(cmd, json!({"protocolVersion": 2}));
            }
            "set_host_tools" => {
                let names: Vec<Value> = cmd["tools"].as_array().unwrap().iter().map(|t| t["name"].clone()).collect();
                self.tools.extend(names.iter().filter_map(|n| n.as_str().map(String::from)));
                ok(cmd, json!({"toolNames": names}));
            }
            "get_state" => {
                let tools: Vec<Value> = self.tools.iter().map(|name| json!({"name": name})).collect();
                ok(
                    cmd,
                    json!({"sessionFile": self.session_file, "isStreaming": false,
                           "messageCount": self.entries.len(), "dumpTools": tools}),
                )
            }
            "get_branch_messages" => {
                let messages: Vec<Value> =
                    self.entries.iter().map(|(id, text)| json!({"entryId": id, "text": text})).collect();
                ok(cmd, json!({"messages": messages}));
            }
            "branch" => {
                let target = cmd["entryId"].as_str().unwrap();
                let pos = self.entries.iter().position(|(id, _)| id == target).unwrap();
                let text = self.entries[pos].1.clone();
                self.entries.truncate(pos);
                self.new_file();
                ok(cmd, json!({"text": text, "cancelled": false}));
            }
            "new_session" => {
                self.entries.clear();
                self.new_file();
                ok(cmd, json!({"cancelled": false}));
            }
            "abort" => ok(cmd, json!(null)),
            "prompt" | "abort_and_prompt" => self.prompt(cmd, lines),
            other => out(&json!({"type": "response", "command": other, "success": false,
                                 "error": format!("unknown command {other}")})),
        }
    }

    fn prompt(&mut self, cmd: &Value, lines: &mut Lines) {
        let message = cmd["message"].as_str().unwrap().to_string();
        let images = cmd["images"].as_array().map_or(0, Vec::len);
        out(&json!({"id": cmd["id"], "type": "response", "command": cmd["type"], "success": true, "data": null}));
        if message.contains("FAIL_LATE") {
            out(&json!({"id": cmd["id"], "type": "response", "command": "prompt", "success": false,
                        "error": "No API key found for fake."}));
            return;
        }
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        self.entries.push((id, message.clone()));
        self.save();

        if message.contains("CRASH") {
            std::process::exit(3);
        }
        if message.contains("CORRUPT") {
            out(&json!({"type": "rpc_chunk", "chunkId": "bad", "index": 0, "count": 2, "byteLength": 4,
                        "data": STANDARD.encode("{}")}));
        }
        out(&json!({"type": "agent_start"}));

        if message.contains("SLOW") {
            out(&text_delta("partial"));
            while let Some(line) = lines.next() {
                let next: Value = serde_json::from_str(&line.unwrap()).unwrap();
                match next["type"].as_str().unwrap() {
                    "abort" => {
                        out(&json!({"type": "agent_end", "messages": [], "isTerminal": true}));
                        ok(&next, json!(null));
                        return;
                    }
                    "abort_and_prompt" => {
                        out(&json!({"type": "agent_end", "messages": [], "isTerminal": true}));
                        return self.prompt(&next, lines);
                    }
                    _ => self.handle(&next, lines),
                }
            }
            return;
        }

        if message.contains("USE_TOOL") {
            out(&json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "repo",
                        "args": {"action": "list"}}));
            out(&json!({"type": "host_tool_call", "id": "h1", "toolCallId": "t1", "toolName": "repo",
                        "arguments": {"action": "list"}}));
            let result = wait_for(lines, "host_tool_result", "h1");
            let text = result["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
            out(&json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "repo",
                        "result": {"content": [{"type": "text", "text": text}]},
                        "isError": result["isError"]}));
            out(&text_delta(&format!("tool said: {text}")));
        } else if message.contains("EXT_UI") {
            out(&json!({"type": "extension_ui_request", "id": "u1", "method": "confirm",
                        "title": "Sure?", "message": "Continue?"}));
            let reply = wait_for(lines, "extension_ui_response", "u1");
            out(&text_delta(&format!("ui cancelled={}", reply["cancelled"])));
        } else if message.contains("BIG") && self.v2 {
            let frame = text_delta(&"B".repeat(5000)).to_string();
            let bytes = frame.as_bytes();
            let parts: Vec<&[u8]> = bytes.chunks(2000).collect();
            for (i, part) in parts.iter().enumerate() {
                out(&json!({"type": "rpc_chunk", "chunkId": "big", "index": i, "count": parts.len(),
                            "byteLength": bytes.len(), "data": STANDARD.encode(part)}));
            }
        } else {
            out(&json!({"type": "message_update",
                        "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "hmm"},
                        "message": {"role": "assistant", "content": []}}));
            out(&text_delta(&format!("echo[{}]: ", self.entries.len())));
            thread::sleep(Duration::from_millis(5));
            out(&text_delta(&format!("{message} images={images}")));
        }

        out(&json!({"type": "message_end", "message": {"role": "assistant", "content": [],
                    "stopReason": "stop",
                    "usage": {"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15}}}));
        out(&json!({"type": "agent_end", "messages": [], "isTerminal": false}));
        out(&json!({"type": "agent_end", "messages": [], "isTerminal": true}));
    }
}

fn wait_for(lines: &mut Lines, kind: &str, id: &str) -> Value {
    for line in lines.by_ref() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if v["type"] == kind && v["id"] == id {
            return v;
        }
    }
    panic!("stdin closed while waiting for {kind}");
}
