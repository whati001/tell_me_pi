//! A running `omp --mode rpc` child: request/response correlation, event broadcast,
//! host-tool dispatch and auto-cancelled extension UI dialogs.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

use super::frame::FrameDecoder;

const MAX_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Synthetic event broadcast when the child's stdout closes.
pub const PROCESS_EXIT_EVENT: &str = "process_exit";

pub struct SpawnSpec {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Sends `host_tool_update` frames for one pending host tool call.
#[derive(Clone)]
pub struct Progress {
    id: String,
    tx: mpsc::UnboundedSender<Value>,
}

impl Progress {
    pub fn update(&self, text: &str) {
        let _ = self.tx.send(json!({
            "type": "host_tool_update",
            "id": self.id,
            "partialResult": { "content": [{ "type": "text", "text": text }] }
        }));
    }
}

pub struct HostToolCall {
    pub tool_name: String,
    pub arguments: Value,
    pub progress: Progress,
    pub cancel: CancellationToken,
}

pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

pub type HostToolFn = Arc<dyn Fn(HostToolCall) -> BoxFuture<'static, ToolOutcome> + Send + Sync>;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

pub struct OmpProcess {
    stdin_tx: mpsc::UnboundedSender<Value>,
    close: CancellationToken,
    pending: PendingMap,
    events: broadcast::Sender<Value>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    child: Mutex<Option<Child>>,
}

impl OmpProcess {
    pub async fn spawn(spec: SpawnSpec, host_tool: Option<HostToolFn>) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(&spec.binary)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", spec.binary.display()))?;
        let mut stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Value>();
        let close = CancellationToken::new();
        let close_writer = close.clone();
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = close_writer.cancelled() => break,
                    frame = stdin_rx.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                let mut line = frame.to_string();
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // dropping `stdin` closes the pipe, which asks omp to exit
        });

        let (events, _) = broadcast::channel(4096);
        let pending: PendingMap = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready_rx) = oneshot::channel();

        let reader = Reader {
            events: events.clone(),
            pending: pending.clone(),
            alive: alive.clone(),
            stdin_tx: stdin_tx.clone(),
            host_tool,
            host_calls: Arc::default(),
            ready_tx: Some(ready_tx),
        };
        tokio::spawn(reader.run(stdout));

        let proc = Arc::new(Self {
            stdin_tx,
            close,
            pending,
            events,
            next_id: AtomicU64::new(1),
            alive,
            child: Mutex::new(Some(child)),
        });

        let ready = tokio::time::timeout(READY_TIMEOUT, ready_rx)
            .await
            .context("omp did not send a ready frame in time")?
            .context("omp exited before it was ready")?;
        let supports_v2 = ready["supportedProtocolVersions"].as_array().is_some_and(|v| v.iter().any(|x| x == 2));
        if supports_v2 {
            proc.request(json!({"type": "negotiate_protocol", "protocolVersion": 2})).await?;
        }
        Ok(proc)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sends a command and waits for its response. Fails on `success: false`.
    pub async fn request(&self, mut command: Value) -> anyhow::Result<Value> {
        let id = format!("px-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        command["id"] = Value::String(id.clone());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        // The reader marks the process dead before clearing `pending`, so a request registered
        // after that clear is caught here instead of waiting for the timeout.
        if !self.is_alive() {
            self.pending.lock().await.remove(&id);
            bail!("omp process is not running");
        }
        if let Err(e) = self.send(command) {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("omp exited while waiting for a response"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("timed out waiting for omp response");
            }
        };
        if response["success"] != true {
            let command = response["command"].as_str().unwrap_or("?");
            let error = response["error"].as_str().unwrap_or("unknown error");
            bail!("omp command {command} failed: {error}");
        }
        Ok(response)
    }

    pub fn send(&self, frame: Value) -> anyhow::Result<()> {
        if self.close.is_cancelled() {
            bail!("omp stdin already closed");
        }
        self.stdin_tx.send(frame).map_err(|_| anyhow!("omp stdin closed"))
    }

    /// Closes stdin, waits up to `grace` for a clean exit, then kills the child.
    pub async fn shutdown(&self, grace: Duration) {
        self.close.cancel();
        let Some(mut child) = self.child.lock().await.take() else { return };
        if tokio::time::timeout(grace, child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

struct Reader {
    events: broadcast::Sender<Value>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    stdin_tx: mpsc::UnboundedSender<Value>,
    host_tool: Option<HostToolFn>,
    host_calls: Arc<std::sync::Mutex<HashMap<String, CancellationToken>>>,
    ready_tx: Option<oneshot::Sender<Value>>,
}

impl Reader {
    async fn run(mut self, stdout: tokio::process::ChildStdout) {
        let mut lines = BufReader::new(stdout).lines();
        let mut decoder = FrameDecoder::new(MAX_REASSEMBLED_BYTES);
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match decoder.push_line(&line) {
                    Ok(Some(frame)) => self.dispatch(frame).await,
                    Ok(None) => {}
                    Err(e) => tracing::warn!("dropping bad omp frame: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("reading omp stdout failed: {e}");
                    break;
                }
            }
        }
        self.alive.store(false, Ordering::SeqCst);
        self.pending.lock().await.clear();
        for token in self.host_calls.lock().expect("host_calls poisoned").values() {
            token.cancel();
        }
        let _ = self.events.send(json!({ "type": PROCESS_EXIT_EVENT }));
    }

    async fn dispatch(&mut self, frame: Value) {
        match frame["type"].as_str().unwrap_or_default() {
            "ready" => {
                if let Some(tx) = self.ready_tx.take() {
                    let _ = tx.send(frame);
                }
            }
            "response" => {
                let waiter = match frame["id"].as_str() {
                    Some(id) => self.pending.lock().await.remove(id),
                    None => None,
                };
                match waiter {
                    Some(tx) => {
                        let _ = tx.send(frame);
                    }
                    // late failures (e.g. async prompt errors) go to the event stream
                    None => {
                        let _ = self.events.send(frame);
                    }
                }
            }
            "host_tool_call" => self.start_host_tool(frame),
            "host_tool_cancel" => {
                if let Some(target) = frame["targetId"].as_str()
                    && let Some(token) = self.host_calls.lock().expect("host_calls poisoned").get(target)
                {
                    token.cancel();
                }
            }
            "extension_ui_request" => {
                // Nobody can answer dialogs; cancel them so the turn never stalls.
                let method = frame["method"].as_str().unwrap_or_default();
                if matches!(method, "select" | "confirm" | "input" | "editor")
                    && let Some(id) = frame["id"].as_str()
                {
                    let _ = self.stdin_tx.send(json!({
                        "type": "extension_ui_response", "id": id, "cancelled": true
                    }));
                }
            }
            _ => {
                let _ = self.events.send(frame);
            }
        }
    }

    fn start_host_tool(&self, frame: Value) {
        let Some(id) = frame["id"].as_str().map(String::from) else { return };
        let stdin_tx = self.stdin_tx.clone();
        let Some(handler) = self.host_tool.clone() else {
            let _ = stdin_tx.send(tool_result(&id, "no host tools are registered", true));
            return;
        };
        let cancel = CancellationToken::new();
        self.host_calls.lock().expect("host_calls poisoned").insert(id.clone(), cancel.clone());
        let host_calls = self.host_calls.clone();
        let call = HostToolCall {
            tool_name: frame["toolName"].as_str().unwrap_or_default().to_string(),
            arguments: frame["arguments"].clone(),
            progress: Progress { id: id.clone(), tx: stdin_tx.clone() },
            cancel,
        };
        tokio::spawn(async move {
            let outcome = handler(call).await;
            host_calls.lock().expect("host_calls poisoned").remove(&id);
            let _ = stdin_tx.send(tool_result(&id, &outcome.text, outcome.is_error));
        });
    }
}

fn tool_result(id: &str, text: &str, is_error: bool) -> Value {
    json!({
        "type": "host_tool_result",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }] },
        "isError": is_error
    })
}
