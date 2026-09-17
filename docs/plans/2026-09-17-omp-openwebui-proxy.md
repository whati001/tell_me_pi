# omp ↔ OpenWebUI Proxy Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build `omp-proxy`, a Rust server that exposes the oh-my-pi (`omp`) agent as an
OpenAI-compatible API for OpenWebUI. It keeps one resumable omp session per chat, gives the agent a
read-only `repo` tool (list/refs/checkout), and ships as a hardened Docker Compose deployment.

**Architecture:**
- axum serves `/v1/models` and `/v1/chat/completions` (SSE and non-streaming).
- A `SessionManager` maps each `X-OpenWebUI-Chat-Id` to a long-lived `omp --mode rpc` child. The
  child speaks newline-delimited JSON over stdio (protocol v2 with `rpc_chunk` reassembly).
- It idles out, and is resumed with `--resume <session file>`.
- The proxy forwards only the newest user message. It detects regenerate and edit by comparing
  hashes of the user messages, and uses omp's `branch` (or a transcript rebuild) to follow them.
- The `repo` tool is an RPC *host tool* implemented in Rust. It keeps shared blobless git mirrors
  and detached worktrees; the git token never reaches omp.

**Tech Stack:**
- Rust 2024: tokio, axum 0.8, serde/serde_json, toml, humantime-serde, nix (privilege drop),
  tokio-util, async-stream.
- git CLI.
- omp v18.2.3 release binary.
- Docker (debian bookworm-slim, tini) and Docker Compose with OpenWebUI.

**Design doc:** `docs/plans/2026-09-17-omp-openwebui-proxy-design.md`

**Verified facts about omp v18.2.3** (checked against the source and the release binary):
- Valid CLI flags: `--mode rpc`, `--cwd`, `--session-dir`, `--resume <path>`, `--model`,
  `--thinking`, `--tools`, `--approval-mode yolo`.
- `--tools` **rejects `ast_grep`** in this release. The default read-only list is
  `read,grep,glob,todo`.
- The ready frame advertises protocol v2. `set_host_tools` is honoured; the active tools become
  `read, grep, glob, todo, repo`. Host tools are activated even with `--tools`
  (`session-tools.ts:2000`).
- `prompt` is acknowledged with `data: null`. A later failure (e.g. `No API key found for
  openai-codex`) arrives as a *second* `response` with the same `id` and `success: false`. omp
  does not store that message.
- `get_state.data.sessionFile` is reported before the file exists (it is written lazily).
- `get_branch_messages` returns `{messages: [{entryId, text}]}` for user messages.
  `branch(entryId)` re-roots the session *before* that user message, into a new session file.
- `tool_execution_end` carries no `args`, only `toolCallId`, `toolName`, `result`, `isError`.
- Assistant `usage` fields: `input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens`.
- Release asset URL: `https://github.com/can1357/oh-my-pi/releases/download/v18.2.3/omp-linux-{x64,arm64}`.

**Deviations from the design doc** (fix the doc in Task 13):
- Default tools are `read, grep, glob, todo` (no `lsp`: no language servers in the image; no
  `ast_grep`: rejected by omp).
- A busy chat is handled with `abort` + wait-until-idle + `prompt`, not `abort_and_prompt`, so
  events from the aborted run cannot leak into the new stream.
- `HOME=/data/home` (on the volume), so omp's agent dir is `/data/home/.omp/agent`. Skills are
  mounted at `/data/home/.omp/agent/skills`. No `PI_CODING_AGENT_DIR` is needed.
- Repo locking is an in-process `tokio::Mutex` per repo (only the proxy runs git), not `flock`.

**Conventions:**
- Run everything from the repo root.
- `cargo fmt` uses `rustfmt.toml` (max width 120).
- Commit after every task. End each commit message with the line
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` (omitted below for brevity).

---

### Task 1: Project skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `rustfmt.toml`, `src/lib.rs`, `src/bin/fake_omp.rs` (placeholder)
- Modify: `src/main.rs` (placeholder), `.gitignore`

**Step 1: Replace `Cargo.toml`**

```toml
[package]
name = "tell_me_pi"
version = "0.1.0"
edition = "2024"

[lib]
path = "src/lib.rs"

[[bin]]
name = "omp-proxy"
path = "src/main.rs"

[[bin]]
name = "fake-omp"
path = "src/bin/fake_omp.rs"

[dependencies]
anyhow = "1"
async-stream = "0.3"
axum = "0.8"
base64 = "0.22"
futures = "0.3"
hex = "0.4"
humantime-serde = "1"
nix = { version = "0.30", features = ["user", "process"] }
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
toml = "0.9"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
reqwest = { version = "0.12", default-features = false, features = ["json"] }
tempfile = "3"
```

**Step 2: Formatting config, placeholders, ignore rules**

`rustfmt.toml`:

```toml
max_width = 120
use_small_heuristics = "Max"
```

`src/lib.rs`: empty file for now; each task adds its `pub mod …;` line.

`src/main.rs` and `src/bin/fake_omp.rs`, both for now:

```rust
fn main() {}
```

`.gitignore`:

```text
/target
/secrets
/proxy.toml
.env
```

**Step 3: Verify it builds**

Run: `cargo build`
Expected: `Finished` with no errors. (The first build downloads the dependencies.)

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock rustfmt.toml .gitignore src/
git commit -m "chore: set up omp-proxy crate skeleton"
```

### Task 2: Configuration (`proxy.toml`)

Loads the TOML config, applies defaults, and rejects write-capable tools, duplicate profiles and unsafe repo names.

**Files:**
- Create: `src/config.rs`
- Modify: `src/lib.rs` (add `pub mod config;`)

**Step 1: Write the failing tests**

Add `pub mod config;` to `src/lib.rs` (keep the modules sorted). Create `src/config.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[profile]]
        name = "omp-codex"
        model = "openai-codex/gpt-5.5"
    "#;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:8080");
        assert_eq!(c.sessions.idle_timeout, Duration::from_secs(900));
        assert_eq!(c.sessions.max_live, 8);
        assert_eq!(c.omp.tools, ["read", "grep", "glob", "todo"]);
        assert_eq!(c.omp.reasoning, ReasoningMode::Field);
        assert_eq!(c.git.token_username, "x-access-token");
        assert_eq!(c.profile("omp-codex").unwrap().model, "openai-codex/gpt-5.5");
        assert!(c.profile("nope").is_none());
    }

    #[test]
    fn full_config_parses() {
        let c = Config::from_toml(
            r#"
            [server]
            listen = "127.0.0.1:9000"
            api_key_file = "/run/secrets/proxy_api_key"

            [sessions]
            data_dir = "/tmp/x"
            idle_timeout = "0s"
            max_live = 2
            turn_timeout = "30s"
            retention = "7d"

            [omp]
            binary = "/usr/local/bin/omp"
            tools = ["read", "grep"]
            extra_args = ["--no-title"]
            env_passthrough = ["OPENAI_API_KEY"]
            reasoning = "think_tags"

            [[profile]]
            name = "a"
            model = "m"
            thinking = "high"

            [git]
            token_file = "/run/secrets/git_token"
            [[git.repo]]
            name = "backend"
            url = "https://github.com/acme/backend.git"
            description = "Backend"
            "#,
        )
        .unwrap();
        assert_eq!(c.sessions.idle_timeout, Duration::ZERO);
        assert_eq!(c.sessions.retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.omp.reasoning, ReasoningMode::ThinkTags);
        assert_eq!(c.repo("backend").unwrap().description, "Backend");
        assert_eq!(c.sessions.mirrors_dir(), PathBuf::from("/tmp/x/mirrors"));
    }

    #[test]
    fn rejects_write_tools() {
        let err = Config::from_toml(&format!("[omp]\ntools = [\"read\", \"bash\"]\n{MINIMAL}")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    #[test]
    fn rejects_missing_profiles_and_bad_repo_names() {
        assert!(Config::from_toml("").is_err());
        let bad = format!("{MINIMAL}\n[[git.repo]]\nname = \"../etc\"\nurl = \"https://x\"\n");
        assert!(Config::from_toml(&bad).is_err());
    }
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: compile errors (`Config`, `Duration`, `ReasoningMode` not found).

**Step 3: Write the implementation**

Insert above the test module in `src/config.rs`:

```rust
//! `proxy.toml` loading and validation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Deserialize;

/// Tools that would let the agent change files or run commands. Rejected in `omp.tools`.
const FORBIDDEN_TOOLS: &[&str] = &["bash", "eval", "edit", "write", "ast_edit", "task", "debug"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub omp: OmpConfig,
    #[serde(rename = "profile")]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: String,
    pub api_key_file: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8080".into(), api_key_file: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionsConfig {
    pub data_dir: PathBuf,
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Duration,
    pub max_live: usize,
    #[serde(with = "humantime_serde")]
    pub turn_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/data"),
            idle_timeout: Duration::from_secs(15 * 60),
            max_live: 8,
            turn_timeout: Duration::from_secs(10 * 60),
            retention: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

impl SessionsConfig {
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    pub fn mirrors_dir(&self) -> PathBuf {
        self.data_dir.join("mirrors")
    }
    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.json")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    /// `delta.reasoning_content`
    #[default]
    Field,
    /// `<think>…</think>` inside `delta.content`
    ThinkTags,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OmpConfig {
    pub binary: PathBuf,
    pub tools: Vec<String>,
    pub extra_args: Vec<String>,
    pub env_passthrough: Vec<String>,
    pub reasoning: ReasoningMode,
}

impl Default for OmpConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("omp"),
            tools: ["read", "grep", "glob", "todo"].map(String::from).to_vec(),
            extra_args: Vec::new(),
            env_passthrough: Vec::new(),
            reasoning: ReasoningMode::Field,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub model: String,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitConfig {
    pub token_file: Option<PathBuf>,
    /// Basic-auth user name sent with the token (GitHub: `x-access-token`, GitLab: `oauth2`).
    pub token_username: String,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self { token_file: None, token_username: "x-access-token".into(), repos: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub description: String,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text).context("invalid proxy config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
        Self::from_toml(&text)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.git.repos.iter().find(|r| r.name == name)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.profiles.is_empty() {
            bail!("at least one [[profile]] is required");
        }
        let mut names = HashSet::new();
        for p in &self.profiles {
            if !names.insert(&p.name) {
                bail!("duplicate profile name {:?}", p.name);
            }
        }
        for tool in &self.omp.tools {
            if FORBIDDEN_TOOLS.contains(&tool.as_str()) {
                bail!("tool {tool:?} is not allowed: the agent must stay read-only");
            }
        }
        let mut repos = HashSet::new();
        for r in &self.git.repos {
            if !is_valid_repo_name(&r.name) {
                bail!("invalid repo name {:?} (use a-z, 0-9, '.', '_', '-')", r.name);
            }
            if !repos.insert(&r.name) {
                bail!("duplicate repo name {:?}", r.name);
            }
        }
        Ok(())
    }
}

fn is_valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}
```

**Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config`
Expected: all tests in the module PASS. `cargo clippy --all-targets` shows no warnings.

**Step 5: Commit**

```bash
git add src/lib.rs src/config.rs
git commit -m "feat: add proxy configuration loading and validation"
```

### Task 3: RPC frame decoder (v2 `rpc_chunk` reassembly)

omp writes one JSON object per stdout line. After `negotiate_protocol` v2, large objects arrive as
`rpc_chunk` frames (`chunkId`, `index`, `count`, `byteLength`, base64 `data`) that must be
validated and reassembled.

**Files:**
- Create: `src/rpc/mod.rs`, `src/rpc/frame.rs`
- Modify: `src/lib.rs` (add `pub mod rpc;`)

**Step 1: Write the failing tests**

`src/rpc/mod.rs` (for now):

```rust
//! Client side of the omp RPC protocol (`omp --mode rpc`).

pub mod frame;
```

`src/rpc/frame.rs`, test module only:

```rust
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

    #[test]
    fn rejects_oversized_frames() {
        let payload = json!({"data": "z".repeat(100)});
        let mut d = FrameDecoder::new(10);
        assert!(d.push_line(&chunks("c", &payload, 2)[0]).is_err());
    }
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --lib rpc::frame`
Expected: compile errors (`FrameDecoder` not found).

**Step 3: Write the implementation** (above the tests)

```rust
//! Decoder for omp RPC stdout lines, including protocol-v2 `rpc_chunk` reassembly.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid rpc_chunk: {0}")]
    Chunk(String),
}

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
    pub fn push_line(&mut self, line: &str) -> Result<Option<Value>, FrameError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(line)?;
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
        let byte_length = v["byteLength"].as_u64().ok_or_else(|| bad("missing byteLength"))? as usize;
        let data = v["data"].as_str().ok_or_else(|| bad("missing data"))?;

        if index == 0 {
            if self.pending.is_some() {
                return Err(bad("new chunk sequence started before the previous one finished"));
            }
            if count == 0 || byte_length > self.max_bytes {
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
        Ok(Some(serde_json::from_str(&text)?))
    }
}
```

**Step 4: Run the tests to verify they pass**

Run: `cargo test --lib rpc::frame`
Expected: 4 tests PASS.

**Step 5: Commit**

```bash
git add src/lib.rs src/rpc
git commit -m "feat: add omp RPC frame decoder with chunk reassembly"
```

### Task 4: Fake omp test double

A small binary that speaks omp's RPC protocol with behaviour scripted by keywords in the prompt.
All integration tests use it, so no LLM or network is needed.

**Files:**
- Modify: `src/bin/fake_omp.rs`

**Step 1: Write the fake**

```rust
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
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let session_dir = PathBuf::from(arg("--session-dir").expect("--session-dir required"));
    std::fs::create_dir_all(&session_dir).unwrap();
    let log = session_dir.parent().unwrap().join("args.log");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(f, "{}", json!(args)).unwrap();

    let mut fake = Fake { session_dir, session_file: PathBuf::new(), entries: Vec::new(), next_entry: 1, v2: false };
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
                ok(cmd, json!({"toolNames": names}));
            }
            "get_state" => ok(
                cmd,
                json!({"sessionFile": self.session_file, "isStreaming": false,
                                            "messageCount": self.entries.len()}),
            ),
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
```

**Step 2: Verify it builds and answers the handshake**

Run:
```bash
cargo build --bin fake-omp
d=$(mktemp -d); printf '%s\n' '{"id":"1","type":"get_state"}' | ./target/debug/fake-omp --session-dir "$d/omp"
```
Expected: a `ready` frame, then a `response` for id `1` with `sessionFile` ending in
`session-0.json`.

**Step 3: Commit**

```bash
git add src/bin/fake_omp.rs
git commit -m "test: add scripted fake omp RPC binary"
```

### Task 5: omp process client

Spawns `omp --mode rpc` and handles the stdio protocol:
- negotiates v2;
- matches responses to requests by `id`; responses with no waiting request (late prompt failures)
  go to the event stream;
- broadcasts all other frames as events;
- runs host tool calls, including progress updates and cancellation;
- auto-cancels extension dialogs;
- emits a synthetic `process_exit` event when stdout closes.

**Files:**
- Create: `src/rpc/process.rs`, `tests/rpc_process.rs`
- Modify: `src/rpc/mod.rs`

**Step 1: Write the failing integration tests**

`tests/rpc_process.rs`:

```rust
use std::{path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tell_me_pi::rpc::{HostToolFn, OmpProcess, PROCESS_EXIT_EVENT, SpawnSpec, ToolOutcome};
use tokio::sync::broadcast;

fn spec(dir: &tempfile::TempDir) -> SpawnSpec {
    SpawnSpec {
        binary: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
        args: vec!["--mode".into(), "rpc".into(), "--session-dir".into(), dir.path().display().to_string()],
        cwd: dir.path().to_path_buf(),
        env: vec![],
    }
}

async fn collect_until_end(rx: &mut broadcast::Receiver<Value>) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        let done = (ev["type"] == "agent_end" && ev["isTerminal"] != false) || ev["type"] == PROCESS_EXIT_EVENT;
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["assistantMessageEvent"]["type"] == "text_delta")
        .map(|e| e["assistantMessageEvent"]["delta"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn prompt_streams_events_and_requests_correlate() {
    let dir = tempfile::tempdir().unwrap();
    let proc = OmpProcess::spawn(spec(&dir), None).await.unwrap();
    let mut rx = proc.subscribe();
    let ack = proc.request(json!({"type": "prompt", "message": "hello"})).await.unwrap();
    assert_eq!(ack["success"], true);
    let events = collect_until_end(&mut rx).await;
    assert_eq!(text_of(&events), "echo[1]: hello images=0");

    let state = proc.request(json!({"type": "get_state"})).await.unwrap();
    assert!(state["data"]["sessionFile"].as_str().unwrap().ends_with(".json"));

    proc.shutdown(Duration::from_secs(2)).await;
    assert!(!proc.is_alive());
}

#[tokio::test]
async fn host_tool_calls_are_dispatched() {
    let dir = tempfile::tempdir().unwrap();
    let handler: HostToolFn = Arc::new(|call| {
        Box::pin(async move {
            call.progress.update("working");
            ToolOutcome { text: format!("{} {}", call.tool_name, call.arguments["action"]), is_error: false }
        })
    });
    let proc = OmpProcess::spawn(spec(&dir), Some(handler)).await.unwrap();
    let mut rx = proc.subscribe();
    proc.request(json!({"type": "prompt", "message": "USE_TOOL"})).await.unwrap();
    let events = collect_until_end(&mut rx).await;
    assert_eq!(text_of(&events), "tool said: repo \"list\"");
}

#[tokio::test]
async fn extension_dialogs_are_cancelled_and_chunks_reassembled() {
    let dir = tempfile::tempdir().unwrap();
    let proc = OmpProcess::spawn(spec(&dir), None).await.unwrap();
    let mut rx = proc.subscribe();
    proc.request(json!({"type": "prompt", "message": "EXT_UI"})).await.unwrap();
    assert_eq!(text_of(&collect_until_end(&mut rx).await), "ui cancelled=true");

    proc.request(json!({"type": "prompt", "message": "BIG"})).await.unwrap();
    assert_eq!(text_of(&collect_until_end(&mut rx).await), "B".repeat(5000));
}

#[tokio::test]
async fn crash_is_reported_as_process_exit() {
    let dir = tempfile::tempdir().unwrap();
    let proc = OmpProcess::spawn(spec(&dir), None).await.unwrap();
    let mut rx = proc.subscribe();
    proc.request(json!({"type": "prompt", "message": "CRASH"})).await.unwrap();
    let events = collect_until_end(&mut rx).await;
    assert_eq!(events.last().unwrap()["type"], PROCESS_EXIT_EVENT);
    assert!(!proc.is_alive());
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --test rpc_process`
Expected: compile errors (`tell_me_pi::rpc::OmpProcess` not found).

**Step 3: Write the implementation**

`src/rpc/mod.rs` (final):

```rust
//! Client side of the omp RPC protocol (`omp --mode rpc`).

pub mod frame;
pub mod process;

pub use process::{HostToolCall, HostToolFn, OmpProcess, PROCESS_EXIT_EVENT, Progress, SpawnSpec, ToolOutcome};
```

`src/rpc/process.rs`:

```rust
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
```

Pitfalls this code already handles (don't "simplify" them away):
- **Closing stdin:** the reader task holds a clone of the stdin sender, so dropping the sender does
  not close the pipe. `shutdown` uses the `close` token instead. Without it every shutdown waits
  for the grace period and then kills.
- **Requests to a dead process:** `request` checks `is_alive()` *after* registering its reply
  slot. The reader marks the process dead before clearing the slots, so a request sent to a dead
  process fails immediately instead of waiting 120 s.

**Step 4: Run the tests to verify they pass**

Run: `cargo test --test rpc_process`
Expected: 4 tests PASS in well under a second.

**Step 5: Commit**

```bash
git add src/rpc tests/rpc_process.rs
git commit -m "feat: add omp RPC process client with host tool dispatch"
```

### Task 6: `repo` host tool (read-only git)

**Files:**
- Create: `src/repo.rs`, `tests/repo_tool.rs`
- Modify: `src/lib.rs` (add `pub mod repo;`)

**Step 1: Write the failing tests**

Add `pub mod repo;` to `src/lib.rs`. Unit test module for `src/repo.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_refs() {
        for good in ["v1.4.2", "main", "release/1.4", "a1b2c3d", "feature_x-y"] {
            validate_ref(good).unwrap();
        }
        for bad in ["", "-x", "--upload-pack=evil", "a..b", "/etc", "x/", "x.lock", "a b", "a;b", ".hidden", "x//y"] {
            assert!(validate_ref(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
```

Integration test `tests/repo_tool.rs`. It uses a local origin over `file://`; `uploadpack.allowFilter`
is required for the blobless clone.

```rust
use std::{path::Path, process::Command};

use serde_json::json;
use tell_me_pi::{config::RepoConfig, repo::RepoTool};
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Origin repo with `app.txt` = "one" at v1.0.0 and "two" at v1.1.0 (and on main).
fn make_origin(root: &Path) -> String {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q"]);
    git(&origin, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(origin.join("app.txt"), "one").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-qm", "one"]);
    git(&origin, &["tag", "v1.0.0"]);
    std::fs::write(origin.join("app.txt"), "two").unwrap();
    git(&origin, &["commit", "-qam", "two"]);
    git(&origin, &["tag", "v1.1.0"]);
    format!("file://{}", origin.display())
}

fn tool(root: &Path, url: String) -> RepoTool {
    let repos = vec![RepoConfig { name: "app".into(), url, description: "Demo app".into() }];
    RepoTool::new(repos, root.join("mirrors"), None)
}

#[tokio::test]
async fn list_refs_and_checkout() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    let list = tool.execute(&work, json!({"action": "list"}), &progress, &cancel).await.unwrap();
    assert_eq!(list, "- app: Demo app");

    let refs = tool.execute(&work, json!({"action": "refs", "repo": "app"}), &progress, &cancel).await.unwrap();
    let lines: Vec<&str> = refs.lines().collect();
    assert_eq!(&lines[..2], ["tag v1.1.0", "tag v1.0.0"], "{refs}");
    assert!(lines.contains(&"branch main"), "{refs}");

    let out = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.0.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert!(out.contains("./app@v1.0.0/"), "{out}");
    assert_eq!(std::fs::read_to_string(work.join("app@v1.0.0/app.txt")).unwrap(), "one");

    tool.execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.1.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(work.join("app@v1.1.0/app.txt")).unwrap(), "two");

    let again = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.0.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert!(again.contains("already checked out"), "{again}");
}

#[tokio::test]
async fn rejects_bad_input() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    for args in [
        json!({"action": "push", "repo": "app"}),
        json!({"action": "checkout", "repo": "other", "ref": "v1.0.0"}),
        json!({"action": "checkout", "repo": "app", "ref": "--upload-pack=touch /tmp/pwned"}),
        json!({"action": "checkout", "repo": "app", "ref": "v9.9.9"}),
    ] {
        let err = tool.execute(&work, args.clone(), &progress, &cancel).await.unwrap_err();
        println!("{args} -> {err:#}");
    }
    let err = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v9.9.9"}), &progress, &cancel)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("action=refs"), "{err}");
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --lib repo && cargo test --test repo_tool`
Expected: compile errors (`RepoTool`, `validate_ref` not found).

**Step 3: Write the implementation** (above the tests in `src/repo.rs`)

```rust
//! The `repo` host tool: read-only access to configured git repositories.
//!
//! Shared blobless mirrors live in `<data>/mirrors/<name>.git`; checkouts are detached
//! worktrees inside the chat's working directory. There is deliberately no push action.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::config::RepoConfig;

pub const TOOL_NAME: &str = "repo";
const FETCH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_REFS: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RepoArgs {
    List,
    Refs {
        repo: String,
    },
    Checkout {
        repo: String,
        #[serde(rename = "ref")]
        git_ref: String,
    },
}

pub struct RepoTool {
    repos: Vec<RepoConfig>,
    mirrors_dir: PathBuf,
    auth_header: Option<String>,
    git: PathBuf,
    locks: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
}

impl RepoTool {
    pub fn new(repos: Vec<RepoConfig>, mirrors_dir: PathBuf, token: Option<(&str, &str)>) -> Self {
        let auth_header =
            token.map(|(user, token)| format!("Authorization: Basic {}", STANDARD.encode(format!("{user}:{token}"))));
        Self { repos, mirrors_dir, auth_header, git: PathBuf::from("git"), locks: Mutex::default() }
    }

    /// The `set_host_tools` definition.
    pub fn definition(&self) -> Value {
        let names: Vec<&str> = self.repos.iter().map(|r| r.name.as_str()).collect();
        json!({
            "name": TOOL_NAME,
            "label": "Repository",
            "description": "Read-only access to the configured git repositories. \
                action=list: show available repositories. \
                action=refs: list tags (newest first) and branches of a repository. \
                action=checkout: check out a tag, branch or commit into ./<repo>@<ref> in the \
                working directory; then use read/grep/glob on that directory. \
                Only check out code when the question needs it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "refs", "checkout"] },
                    "repo": { "type": "string", "enum": names },
                    "ref": { "type": "string", "description": "Tag, branch or commit for checkout" }
                },
                "required": ["action"],
                "additionalProperties": false
            },
            "loadMode": "essential"
        })
    }

    /// Runs one tool call. `Ok` text goes to the model; `Err` is reported as a tool error.
    pub async fn execute(
        &self,
        workdir: &Path,
        args: Value,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let args: RepoArgs = serde_json::from_value(args).context("invalid arguments")?;
        match args {
            RepoArgs::List => Ok(self.list()),
            RepoArgs::Refs { repo } => self.refs(&repo, progress, cancel).await,
            RepoArgs::Checkout { repo, git_ref } => self.checkout(workdir, &repo, &git_ref, progress, cancel).await,
        }
    }

    fn list(&self) -> String {
        if self.repos.is_empty() {
            return "No repositories are configured.".into();
        }
        self.repos.iter().map(|r| format!("- {}: {}", r.name, r.description)).collect::<Vec<_>>().join("\n")
    }

    async fn refs(
        &self,
        name: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let mirror = self.sync_mirror(name, progress, cancel).await?;
        let tags = self
            .git(
                Some(&mirror),
                &["for-each-ref", "--sort=-version:refname", "--format=%(refname:short)", "refs/tags"],
                cancel,
            )
            .await?;
        let branches = self
            .git(
                Some(&mirror),
                &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads"],
                cancel,
            )
            .await?;
        let mut lines: Vec<String> =
            tags.lines().map(|t| format!("tag {t}")).chain(branches.lines().map(|b| format!("branch {b}"))).collect();
        if lines.len() > MAX_REFS {
            let more = lines.len() - MAX_REFS;
            lines.truncate(MAX_REFS);
            lines.push(format!("… {more} more"));
        }
        Ok(lines.join("\n"))
    }

    async fn checkout(
        &self,
        workdir: &Path,
        name: &str,
        git_ref: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        validate_ref(git_ref)?;
        let dir_name = format!("{name}@{}", git_ref.replace('/', "_"));
        let target = workdir.join(&dir_name);
        if target.join(".git").exists() {
            return Ok(format!("{name} {git_ref} is already checked out in ./{dir_name}/"));
        }
        let mirror = self.sync_mirror(name, progress, cancel).await?;
        let lock = self.lock_for(name).await;
        let _guard = lock.lock().await;
        let commit = self
            .git(Some(&mirror), &["rev-parse", "--verify", "--quiet", &format!("{git_ref}^{{commit}}")], cancel)
            .await
            .map_err(|_| anyhow::anyhow!("unknown ref {git_ref:?} in {name}; use action=refs to list refs"))?;
        let commit = commit.trim();
        progress(&format!("checking out {name} {git_ref}"));
        let target_str = target.to_str().context("working directory is not valid UTF-8")?;
        self.git(Some(&mirror), &["worktree", "add", "--detach", target_str, commit], cancel).await?;
        Ok(format!(
            "Checked out {name} {git_ref} ({}) into ./{dir_name}/. Use read, grep and glob on paths under ./{dir_name}/.",
            &commit[..commit.len().min(12)]
        ))
    }

    async fn lock_for(&self, name: &str) -> Arc<Mutex<Option<Instant>>> {
        self.locks.lock().await.entry(name.to_string()).or_default().clone()
    }

    /// Clones or fetches the shared mirror. Fetches at most once per `FETCH_INTERVAL`.
    async fn sync_mirror(
        &self,
        name: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<PathBuf> {
        let Some(repo) = self.repos.iter().find(|r| r.name == name) else {
            bail!("unknown repository {name:?}; use action=list");
        };
        let mirror = self.mirrors_dir.join(format!("{name}.git"));
        let lock = self.lock_for(name).await;
        let mut last_fetch = lock.lock().await;
        if !mirror.join("HEAD").exists() {
            progress(&format!("cloning {name} (first use, this can take a while)"));
            tokio::fs::create_dir_all(&self.mirrors_dir).await?;
            let tmp = self.mirrors_dir.join(format!(".{name}.git.tmp"));
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            let tmp_str = tmp.to_str().context("mirror path is not valid UTF-8")?;
            self.git(None, &["clone", "--mirror", "--filter=blob:none", "--", &repo.url, tmp_str], cancel).await?;
            tokio::fs::rename(&tmp, &mirror).await?;
            *last_fetch = Some(Instant::now());
        } else if last_fetch.is_none_or(|t| t.elapsed() > FETCH_INTERVAL) {
            progress(&format!("fetching {name}"));
            self.git(Some(&mirror), &["fetch", "--prune", "--tags", "origin"], cancel).await?;
            self.git(Some(&mirror), &["worktree", "prune"], cancel).await?;
            *last_fetch = Some(Instant::now());
        }
        Ok(mirror)
    }

    async fn git(&self, git_dir: Option<&Path>, args: &[&str], cancel: &CancellationToken) -> anyhow::Result<String> {
        let mut cmd = Command::new(&self.git);
        if let Some(dir) = git_dir {
            cmd.arg("--git-dir").arg(dir);
        }
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.mirrors_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(header) = &self.auth_header {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        let child = cmd.spawn().context("spawning git")?;
        let output = tokio::select! {
            out = child.wait_with_output() => out.context("running git")?,
            _ = cancel.cancelled() => bail!("cancelled"),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args[0], stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Accepts tag, branch and commit names; rejects option injection and path tricks.
pub fn validate_ref(r: &str) -> anyhow::Result<()> {
    let ok = !r.is_empty()
        && r.len() <= 200
        && !r.starts_with(['-', '/', '.'])
        && !r.ends_with(['/', '.'])
        && !r.ends_with(".lock")
        && !r.contains("..")
        && !r.contains("//")
        && r.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
    if !ok {
        bail!("invalid ref {r:?}");
    }
    Ok(())
}
```

Notes:
- Tags are sorted by `-version:refname` so `v1.10.0` comes before `v1.9.0`.
- The token goes into `GIT_CONFIG_*` environment variables of the git child only.
- `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` isolate git from any host config.

**Step 4: Run the tests to verify they pass**

Run: `cargo test --lib repo && cargo test --test repo_tool`
Expected: 1 + 2 tests PASS.

**Step 5: Commit**

```bash
git add src/lib.rs src/repo.rs tests/repo_tool.rs
git commit -m "feat: add read-only repo host tool with shared mirrors"
```

### Task 7: Chat history planning

Parses OpenAI messages (text and data-URL images) and decides, from hashes of the user messages already sent, whether a request is a normal follow-up (`Prompt`), a regenerate or edit (`BranchAt(i)`), or needs a transcript rebuild (`Rebuild`). `find_entry` maps a sent turn to omp's `entryId` by matching the stored text in order.

**Files:**
- Create: `src/history.rs`
- Modify: `src/lib.rs` (add `pub mod history;`)

**Step 1: Write the failing tests**

Add `pub mod history;` to `src/lib.rs` (keep the modules sorted). Create `src/history.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage { role: role.into(), content: Some(MessageContent::Text(text.into())) }
    }

    fn fwd(turns: &[UserTurn]) -> Vec<ForwardedTurn> {
        turns.iter().map(|t| ForwardedTurn { hash: t.hash.clone(), sent_text: Some(t.text.clone()) }).collect()
    }

    #[test]
    fn extracts_text_and_images() {
        let m: ChatMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
            ]
        }))
        .unwrap();
        let turns = user_turns(&[msg("system", "sys"), m]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "what is this");
        assert_eq!(turns[0].images, vec![json!({"type": "image", "mimeType": "image/png", "data": "AAAA"})]);

        let empty = user_turns(&[ChatMessage { role: "user".into(), content: None }]);
        assert_eq!(empty[0].text, "(see attached image)");
    }

    #[test]
    fn plans_follow_up_regenerate_edit_and_rebuild() {
        let t = user_turns(&[msg("user", "a"), msg("user", "b"), msg("user", "c"), msg("user", "x")]);
        let (a, b, c, x) = (&t[0], &t[1], &t[2], &t[3]);
        let one = |t: &UserTurn| vec![t.clone()];

        assert_eq!(plan(&[], &one(a)), Plan::Prompt);
        assert_eq!(plan(&fwd(&one(a)), &[a.clone(), b.clone()]), Plan::Prompt);
        // regenerate the last answer: same history again
        assert_eq!(plan(&fwd(&[a.clone(), b.clone()]), &[a.clone(), b.clone()]), Plan::BranchAt(1));
        // edit the last message
        assert_eq!(plan(&fwd(&[a.clone(), b.clone()]), &[a.clone(), x.clone()]), Plan::BranchAt(1));
        // edit an earlier message (OpenWebUI drops everything after it)
        assert_eq!(plan(&fwd(&[a.clone(), b.clone(), c.clone()]), &one(x)), Plan::BranchAt(0));
        // proxy never saw this chat (e.g. model switched)
        assert_eq!(plan(&[], &[a.clone(), b.clone()]), Plan::Rebuild);
        // history differs in the middle
        assert_eq!(plan(&fwd(&[a.clone(), b.clone()]), &[x.clone(), b.clone(), c.clone()]), Plan::Rebuild);
        assert_eq!(plan(&[], &[]), Plan::Rebuild);
    }

    #[test]
    fn finds_entries_by_sent_text_in_order() {
        let known = vec![
            ForwardedTurn { hash: "1".into(), sent_text: None },
            ForwardedTurn { hash: "2".into(), sent_text: Some("again".into()) },
            ForwardedTurn { hash: "3".into(), sent_text: Some("again".into()) },
        ];
        let entries = vec![
            ("e0".to_string(), "transcript…".to_string()),
            ("e1".to_string(), "again".to_string()),
            ("e2".to_string(), "again".to_string()),
        ];
        assert_eq!(find_entry(&known, 2, &entries), Some("e2".into()));
        assert_eq!(find_entry(&known, 1, &entries), Some("e1".into()));
        assert_eq!(find_entry(&known, 0, &entries), None);
        assert_eq!(find_entry(&known, 2, &entries[..2]), None);
    }

    #[test]
    fn rebuild_prompt_includes_prior_turns_without_tool_details() {
        let messages = vec![
            msg("system", "ignored"),
            msg("user", "first"),
            msg("assistant", "answer<details><summary>🔧 read</summary>x</details> done"),
            msg("user", "second"),
        ];
        let turns = user_turns(&messages);
        let p = rebuild_prompt(&messages, &turns[1]);
        assert!(p.contains("User: first"), "{p}");
        assert!(p.contains("Assistant: answer done"), "{p}");
        assert!(!p.contains("🔧"), "{p}");
        assert!(p.ends_with("Current request:\nsecond"), "{p}");
        assert_eq!(rebuild_prompt(&messages[3..], &turns[1]), "second");
    }
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --lib history`
Expected: compile errors (`ChatMessage`, `user_turns`, `plan` not found).

**Step 3: Write the implementation**

Insert above the test module in `src/history.rs`:

```rust
//! OpenAI request messages → user turns, and deciding how an incoming history maps onto the
//! omp session (normal follow-up, regenerate/edit via `branch`, or rebuild).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

impl ChatMessage {
    pub fn text(&self) -> String {
        match &self.content {
            None => String::new(),
            Some(MessageContent::Text(t)) => t.clone(),
            Some(MessageContent::Parts(parts)) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn image_urls(&self) -> Vec<&str> {
        match &self.content {
            Some(MessageContent::Parts(parts)) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ImageUrl { image_url } => Some(image_url.url.as_str()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserTurn {
    pub hash: String,
    /// Text sent to omp; never empty.
    pub text: String,
    /// omp `ImageContent` objects.
    pub images: Vec<Value>,
}

/// One user turn the proxy already handled for a chat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardedTurn {
    pub hash: String,
    /// Exact text omp received for this turn, or `None` if the turn only reached omp inside a
    /// rebuild transcript.
    pub sent_text: Option<String>,
}

pub fn user_turns(messages: &[ChatMessage]) -> Vec<UserTurn> {
    messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| {
            let urls = m.image_urls();
            let mut hasher = Sha256::new();
            hasher.update(m.text().as_bytes());
            for url in &urls {
                hasher.update([0]);
                hasher.update(url.as_bytes());
            }
            let images: Vec<Value> = urls.iter().filter_map(|u| data_url_image(u)).collect();
            let mut text = m.text();
            if text.trim().is_empty() {
                text = "(see attached image)".into();
            }
            UserTurn { hash: hex::encode(hasher.finalize()), text, images }
        })
        .collect()
}

fn data_url_image(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    mime.starts_with("image/").then(|| json!({"type": "image", "mimeType": mime, "data": data}))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Known history plus one new user message.
    Prompt,
    /// History up to the last user message is known, but that message was regenerated or edited:
    /// go back to before forwarded turn `index`, then prompt.
    BranchAt(usize),
    /// History diverges in a way omp can't follow: start over with a transcript.
    Rebuild,
}

pub fn plan(known: &[ForwardedTurn], incoming: &[UserTurn]) -> Plan {
    let Some((_, prefix)) = incoming.split_last() else { return Plan::Rebuild };
    let prefix_matches =
        |len: usize| known.len() >= len && known[..len].iter().zip(prefix).all(|(k, i)| k.hash == i.hash);
    if known.len() == prefix.len() && prefix_matches(prefix.len()) {
        return Plan::Prompt;
    }
    if known.len() > prefix.len() && prefix_matches(prefix.len()) {
        return Plan::BranchAt(prefix.len());
    }
    Plan::Rebuild
}

/// Finds the omp entry id of forwarded turn `index`, given omp's `get_branch_messages` list.
pub fn find_entry(known: &[ForwardedTurn], index: usize, entries: &[(String, String)]) -> Option<String> {
    known.get(index)?.sent_text.as_ref()?;
    let mut entries = entries.iter();
    for (i, turn) in known.iter().enumerate() {
        let Some(sent) = &turn.sent_text else { continue };
        let (id, _) = entries.by_ref().find(|(_, text)| text == sent)?;
        if i == index {
            return Some(id.clone());
        }
    }
    None
}

/// Prompt for a fresh omp session that carries the earlier conversation as context.
pub fn rebuild_prompt(messages: &[ChatMessage], last: &UserTurn) -> String {
    let details = regex::Regex::new(r"(?s)<details.*?</details>").expect("valid regex");
    let last_user = messages.iter().rposition(|m| m.role == "user").unwrap_or(0);
    let mut transcript = String::new();
    for m in &messages[..last_user] {
        let who = match m.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            _ => continue,
        };
        let raw = m.text();
        let text = details.replace_all(&raw, "");
        transcript.push_str(&format!("{who}: {}\n\n", text.trim()));
    }
    if transcript.is_empty() {
        return last.text.clone();
    }
    format!(
        "Earlier conversation, for context only:\n<conversation>\n{}</conversation>\n\nCurrent request:\n{}",
        transcript, last.text
    )
}
```

**Step 4: Run the tests to verify they pass**

Run: `cargo test --lib history`
Expected: all tests in the module PASS. `cargo clippy --all-targets` shows no warnings.

**Step 5: Commit**

```bash
git add src/lib.rs src/history.rs
git commit -m "feat: add chat history planning for follow-up, regenerate and edit"
```

### Task 8: omp event → OpenAI chunk translator

Maps `thinking_delta` → reasoning, `text_delta` → content, tool executions → a collapsible `<details>` block (the summary is remembered from `tool_execution_start`), terminal `agent_end` → done, and late failures or process exit → errors. It also accumulates usage.

**Files:**
- Create: `src/translate.rs`
- Modify: `src/lib.rs` (add `pub mod translate;`)

**Step 1: Write the failing tests**

Add `pub mod translate;` to `src/lib.rs` (keep the modules sorted). Create `src/translate.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(kind: &str, text: &str) -> Value {
        json!({"type": "message_update", "assistantMessageEvent": {"type": kind, "delta": text}})
    }

    #[test]
    fn maps_text_and_reasoning_field() {
        let mut t = Translator::new(ReasoningMode::Field);
        assert_eq!(t.on_event(&delta("thinking_delta", "hm")), vec![Out::Reasoning("hm".into())]);
        assert_eq!(t.on_event(&delta("text_delta", "hi")), vec![Out::Content("hi".into())]);
        assert_eq!(t.on_event(&delta("toolcall_delta", "{")), vec![]);
    }

    #[test]
    fn wraps_thinking_in_tags_when_configured() {
        let mut t = Translator::new(ReasoningMode::ThinkTags);
        let mut out = t.on_event(&delta("thinking_delta", "hm"));
        out.extend(t.on_event(&delta("text_delta", "hi")));
        assert_eq!(
            out,
            vec![
                Out::Content("<think>\n".into()),
                Out::Content("hm".into()),
                Out::Content("\n</think>\n\n".into()),
                Out::Content("hi".into())
            ]
        );
        let mut off = Translator::new(ReasoningMode::Off);
        assert!(off.on_event(&delta("thinking_delta", "hm")).is_empty());
    }

    #[test]
    fn only_terminal_agent_end_finishes() {
        let mut t = Translator::new(ReasoningMode::Field);
        assert!(t.on_event(&json!({"type": "agent_end", "isTerminal": false})).is_empty());
        assert_eq!(t.on_event(&json!({"type": "agent_end", "isTerminal": true})), vec![Out::Done]);
        assert_eq!(t.on_event(&json!({"type": "agent_end"})), vec![Out::Done]);
        assert_eq!(t.on_event(&json!({"type": "prompt_result", "agentInvoked": false})), vec![Out::Done]);
    }

    #[test]
    fn accumulates_usage_and_reports_errors() {
        let mut t = Translator::new(ReasoningMode::Field);
        let end = json!({"type": "message_end", "message": {"role": "assistant", "stopReason": "error",
            "errorMessage": "rate limited", "usage": {"input": 10, "output": 5, "cacheRead": 2, "cacheWrite": 0}}});
        assert_eq!(t.on_event(&end), vec![Out::Content("\n\n⚠️ rate limited\n".into())]);
        t.on_event(&end);
        assert_eq!(t.usage, Usage { prompt_tokens: 24, completion_tokens: 10, total_tokens: 34 });

        let fail = json!({"type": "response", "command": "prompt", "success": false, "error": "boom"});
        assert_eq!(t.on_event(&fail), vec![Out::Error("boom".into())]);
        assert!(matches!(t.on_event(&json!({"type": PROCESS_EXIT_EVENT}))[..], [Out::Error(_)]));
    }

    #[test]
    fn renders_tool_calls() {
        let mut t = Translator::new(ReasoningMode::Field);
        let start = json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "repo",
                           "args": {"action": "checkout", "repo": "backend", "ref": "v1.2"}});
        assert_eq!(t.on_event(&start), vec![Out::Reasoning("\n🔧 repo checkout backend v1.2\n".into())]);

        let start = json!({"type": "tool_execution_start", "toolCallId": "t2", "toolName": "grep",
                           "args": {"pattern": "fn main", "path": "src"}});
        t.on_event(&start);
        let end = json!({"type": "tool_execution_end", "toolCallId": "t2", "toolName": "grep", "isError": false,
                         "result": {"content": [{"type": "text", "text": "src/main.rs:1"}]}});
        let Out::Content(html) = &t.on_event(&end)[0] else { panic!() };
        assert!(html.contains("<summary>✓ grep fn main in src</summary>"), "{html}");
        assert!(html.contains("src/main.rs:1"), "{html}");
    }
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --lib translate`
Expected: compile errors (`Translator`, `Out` not found (also needs `rpc::PROCESS_EXIT_EVENT` from Task 5)).

**Step 3: Write the implementation**

Insert above the test module in `src/translate.rs`:

```rust
//! omp session events → pieces of an OpenAI chat completion.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::{config::ReasoningMode, rpc::PROCESS_EXIT_EVENT};

const MAX_TOOL_OUTPUT: usize = 1500;

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Content(String),
    Reasoning(String),
    /// The turn finished.
    Done,
    /// The turn failed; the stream must end.
    Error(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

pub struct Translator {
    reasoning: ReasoningMode,
    in_think: bool,
    /// toolCallId → one-line summary (end events carry no arguments)
    tools: HashMap<String, String>,
    pub usage: Usage,
}

impl Translator {
    pub fn new(reasoning: ReasoningMode) -> Self {
        Self { reasoning, in_think: false, tools: HashMap::new(), usage: Usage::default() }
    }

    pub fn on_event(&mut self, ev: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        match ev["type"].as_str().unwrap_or_default() {
            "message_update" => {
                let inner = &ev["assistantMessageEvent"];
                let delta = inner["delta"].as_str().unwrap_or_default();
                match inner["type"].as_str().unwrap_or_default() {
                    "thinking_delta" => self.thinking(delta, &mut out),
                    "text_delta" => {
                        self.close_think(&mut out);
                        out.push(Out::Content(delta.to_string()));
                    }
                    _ => {}
                }
            }
            "message_end" => {
                let msg = &ev["message"];
                if msg["role"] == "assistant" {
                    let u = &msg["usage"];
                    let input = u["input"].as_u64().unwrap_or(0)
                        + u["cacheRead"].as_u64().unwrap_or(0)
                        + u["cacheWrite"].as_u64().unwrap_or(0);
                    let output = u["output"].as_u64().unwrap_or(0);
                    self.usage.prompt_tokens += input;
                    self.usage.completion_tokens += output;
                    self.usage.total_tokens += input + output;
                    if msg["stopReason"] == "error"
                        && let Some(err) = msg["errorMessage"].as_str()
                    {
                        self.close_think(&mut out);
                        out.push(Out::Content(format!("\n\n⚠️ {err}\n")));
                    }
                }
            }
            "tool_execution_start" => {
                let summary = tool_summary(&ev["toolName"], &ev["args"]);
                if self.reasoning == ReasoningMode::Field {
                    out.push(Out::Reasoning(format!("\n🔧 {summary}\n")));
                }
                let id = ev["toolCallId"].as_str().unwrap_or_default().to_string();
                self.tools.insert(id, summary);
            }
            "tool_execution_end" => {
                let summary = ev["toolCallId"]
                    .as_str()
                    .and_then(|id| self.tools.remove(id))
                    .unwrap_or_else(|| ev["toolName"].as_str().unwrap_or("tool").to_string());
                self.close_think(&mut out);
                out.push(Out::Content(tool_details(ev, &summary)));
            }
            "command_output" => {
                self.close_think(&mut out);
                out.push(Out::Content(ev["text"].as_str().unwrap_or_default().to_string()));
            }
            "prompt_result" if ev["agentInvoked"] == false => {
                self.close_think(&mut out);
                out.push(Out::Done);
            }
            "agent_end" if ev["isTerminal"] != false => {
                self.close_think(&mut out);
                out.push(Out::Done);
            }
            "response" if ev["success"] == false => {
                let err = ev["error"].as_str().unwrap_or("omp command failed");
                out.push(Out::Error(err.to_string()));
            }
            t if t == PROCESS_EXIT_EVENT => out.push(Out::Error("the agent process exited unexpectedly".into())),
            _ => {}
        }
        out
    }

    fn thinking(&mut self, delta: &str, out: &mut Vec<Out>) {
        match self.reasoning {
            ReasoningMode::Field => out.push(Out::Reasoning(delta.to_string())),
            ReasoningMode::ThinkTags => {
                if !self.in_think {
                    self.in_think = true;
                    out.push(Out::Content("<think>\n".into()));
                }
                out.push(Out::Content(delta.to_string()));
            }
            ReasoningMode::Off => {}
        }
    }

    fn close_think(&mut self, out: &mut Vec<Out>) {
        if self.in_think {
            self.in_think = false;
            out.push(Out::Content("\n</think>\n\n".into()));
        }
    }
}

fn tool_summary(name: &Value, args: &Value) -> String {
    let name = name.as_str().unwrap_or("tool");
    let s = |k: &str| args[k].as_str().unwrap_or_default();
    let detail = match name {
        "repo" => {
            [s("action"), s("repo"), s("ref")].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(" ")
        }
        "read" => s("path").to_string(),
        "grep" | "glob" | "ast_grep" => {
            [s("pattern"), s("path")].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(" in ")
        }
        _ => truncate(&args.to_string(), 80),
    };
    format!("{name} {detail}").trim_end().to_string()
}

fn tool_details(ev: &Value, summary: &str) -> String {
    let failed = ev["isError"] == true;
    let mark = if failed { "✗" } else { "✓" };
    let text: String = ev["result"]["content"]
        .as_array()
        .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    format!(
        "\n<details>\n<summary>{mark} {summary}</summary>\n\n```\n{}\n```\n</details>\n\n",
        truncate(&text, MAX_TOOL_OUTPUT).replace("```", "ˋˋˋ")
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}
```

**Step 4: Run the tests to verify they pass**

Run: `cargo test --lib translate`
Expected: all tests in the module PASS. `cargo clippy --all-targets` shows no warnings.

**Step 5: Commit**

```bash
git add src/lib.rs src/translate.rs
git commit -m "feat: translate omp session events into OpenAI chunks"
```

### Task 9: Session manager and HTTP API

These two modules are written together. The HTTP tests exercise the session manager end to end
through the fake omp.

**Files:**
- Create: `src/session.rs`, `src/api.rs`, `tests/api.rs`
- Modify: `src/lib.rs` (add `pub mod api;` and `pub mod session;`)

**Step 1: Write the failing end-to-end tests**

`tests/api.rs`:

```rust
//! End-to-end tests: HTTP API → session manager → fake omp.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tell_me_pi::{
    api::{self, AppState},
    config::{Config, RepoConfig},
    repo::RepoTool,
    session::SessionManager,
};

struct Server {
    url: String,
    data: tempfile::TempDir,
    sessions: Arc<SessionManager>,
    client: reqwest::Client,
}

async fn start(extra_sessions_config: &str) -> Server {
    let data = tempfile::tempdir().unwrap();
    let cfg = Config::from_toml(&format!(
        r#"
        [sessions]
        data_dir = "{data}"
        {extra_sessions_config}

        [omp]
        binary = "{bin}"

        [[profile]]
        name = "omp-test"
        model = "fake/model"
        thinking = "low"
        "#,
        data = data.path().display(),
        bin = env!("CARGO_BIN_EXE_fake-omp"),
    ))
    .unwrap();
    let cfg = Arc::new(cfg);
    let repos = vec![RepoConfig { name: "app".into(), url: "file:///nonexistent".into(), description: "Demo".into() }];
    let repo = Arc::new(RepoTool::new(repos, cfg.sessions.mirrors_dir(), None));
    let sessions = SessionManager::new(cfg.clone(), repo).unwrap();
    let app = api::router(Arc::new(AppState { cfg, sessions: sessions.clone(), api_key: Some("secret".into()) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, data, sessions, client: reqwest::Client::new() }
}

#[derive(Debug, Default)]
struct Reply {
    content: String,
    reasoning: String,
    errors: Vec<String>,
    finished: bool,
    usage: Value,
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

impl Server {
    fn request(&self, chat: &str, messages: &[Value], stream: bool) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth("secret")
            .header("X-OpenWebUI-Chat-Id", chat)
            .json(&json!({"model": "omp-test", "messages": messages, "stream": stream}))
    }

    async fn chat(&self, chat: &str, messages: &[Value]) -> Reply {
        let resp = self.request(chat, messages, true).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        parse_sse(&resp.text().await.unwrap())
    }

    fn args_logs(&self) -> Vec<Vec<String>> {
        let mut all = Vec::new();
        for dir in std::fs::read_dir(self.data.path().join("sessions")).unwrap() {
            let log = dir.unwrap().path().join("args.log");
            for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
                all.push(serde_json::from_str(line).unwrap());
            }
        }
        all
    }
}

fn parse_sse(body: &str) -> Reply {
    let mut reply = Reply::default();
    let mut saw_done = false;
    for data in body.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let v: Value = serde_json::from_str(data).unwrap();
        if let Some(msg) = v["error"]["message"].as_str() {
            reply.errors.push(msg.to_string());
            continue;
        }
        if !v["usage"].is_null() {
            reply.usage = v["usage"].clone();
        }
        let Some(choice) = v["choices"].get(0) else { continue };
        reply.content.push_str(choice["delta"]["content"].as_str().unwrap_or_default());
        reply.reasoning.push_str(choice["delta"]["reasoning_content"].as_str().unwrap_or_default());
        if choice["finish_reason"] == "stop" {
            reply.finished = true;
        }
    }
    assert!(saw_done, "stream must end with [DONE]: {body}");
    reply
}

#[tokio::test]
async fn models_health_and_auth() {
    let s = start("").await;
    let health = s.client.get(format!("{}/healthz", s.url)).send().await.unwrap();
    assert_eq!(health.status(), 200);

    let denied = s.client.get(format!("{}/v1/models", s.url)).send().await.unwrap();
    assert_eq!(denied.status(), 401);

    let models: Value =
        s.client.get(format!("{}/v1/models", s.url)).bearer_auth("secret").send().await.unwrap().json().await.unwrap();
    assert_eq!(models["data"][0]["id"], "omp-test");

    let unknown = s
        .client
        .post(format!("{}/v1/chat/completions", s.url))
        .bearer_auth("secret")
        .json(&json!({"model": "nope", "messages": [user("hi")]}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

#[tokio::test]
async fn follow_up_regenerate_and_edit() {
    let s = start("").await;
    let r1 = s.chat("c1", &[user("hello")]).await;
    assert_eq!(r1.content, "echo[1]: hello images=0");
    assert_eq!(r1.reasoning, "hmm");
    assert!(r1.finished);
    assert_eq!(r1.usage["total_tokens"], 15);

    let history = [user("hello"), assistant(&r1.content), user("more")];
    assert_eq!(s.chat("c1", &history).await.content, "echo[2]: more images=0");

    // regenerate: identical history → omp branches back and still has 2 user messages
    assert_eq!(s.chat("c1", &history).await.content, "echo[2]: more images=0");

    // edit the first message: OpenWebUI sends only the edited message
    assert_eq!(s.chat("c1", &[user("edited")]).await.content, "echo[1]: edited images=0");

    // one omp process served the whole chat
    assert_eq!(s.args_logs().len(), 1);
    let args = &s.args_logs()[0];
    for expected in ["--mode", "rpc", "--model", "fake/model", "--thinking", "low", "--approval-mode", "yolo"] {
        assert!(args.contains(&expected.to_string()), "{args:?}");
    }
    let tools = &args[args.iter().position(|a| a == "--tools").unwrap() + 1];
    assert_eq!(tools, "read,grep,glob,todo");
}

#[tokio::test]
async fn unknown_history_is_rebuilt_as_transcript() {
    let s = start("").await;
    let history = [user("first"), assistant("answer"), user("second")];
    let reply = s.chat("c2", &history).await;
    assert!(reply.content.starts_with("echo[1]: Earlier conversation"), "{}", reply.content);
    assert!(reply.content.contains("User: first"), "{}", reply.content);
    assert!(reply.content.ends_with("Current request:\nsecond images=0"), "{}", reply.content);

    // follow-ups continue normally afterwards
    let next = [user("first"), assistant("answer"), user("second"), assistant("x"), user("third")];
    assert_eq!(s.chat("c2", &next).await.content, "echo[2]: third images=0");
}

#[tokio::test]
async fn non_streaming_with_host_tool_and_images() {
    let s = start("").await;
    let resp: Value = s.request("c3", &[user("USE_TOOL")], false).send().await.unwrap().json().await.unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("<summary>✓ repo list</summary>"), "{content}");
    assert!(content.ends_with("tool said: - app: Demo"), "{content}");
    assert_eq!(resp["object"], "chat.completion");

    let image = json!({"role": "user", "content": [
        {"type": "text", "text": "look"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
    ]});
    let reply = s.chat("c4", &[image]).await;
    assert_eq!(reply.content, "echo[1]: look images=1");
}

#[tokio::test]
async fn new_message_while_busy_aborts_previous_turn() {
    let s = Arc::new(start("").await);
    let slow = {
        let s = s.clone();
        tokio::spawn(async move { s.chat("c5", &[user("SLOW")]).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = s.chat("c5", &[user("SLOW"), assistant("partial"), user("next")]).await;
    assert_eq!(reply.content, "echo[2]: next images=0");
    let first = slow.await.unwrap();
    assert_eq!(first.content, "partial");
    assert!(first.finished);
}

#[tokio::test]
async fn client_disconnect_aborts_and_chat_continues() {
    let s = start("").await;
    let mut resp = s.request("c6", &[user("SLOW")], true).send().await.unwrap();
    resp.chunk().await.unwrap();
    drop(resp);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = s.chat("c6", &[user("SLOW"), assistant("partial"), user("again")]).await;
    assert_eq!(reply.content, "echo[2]: again images=0");
}

#[tokio::test]
async fn turn_timeout_reports_error() {
    let s = start(r#"turn_timeout = "1s""#).await;
    let reply = s.chat("c7", &[user("SLOW")]).await;
    assert_eq!(reply.errors, ["the turn timed out"]);
    let next = s.chat("c7", &[user("SLOW"), assistant("partial"), user("ok")]).await;
    assert_eq!(next.content, "echo[2]: ok images=0");
}

#[tokio::test]
async fn late_prompt_failure_is_reported() {
    let s = start("").await;
    let reply = s.chat("c10", &[user("FAIL_LATE")]).await;
    assert_eq!(reply.errors, ["No API key found for fake."]);
    // omp never stored the failed prompt, so the retry is sent again instead of being skipped
    assert_eq!(s.chat("c10", &[user("again")]).await.content, "echo[1]: again images=0");
    let reply = s.chat("c10", &[user("again"), assistant("x"), user("fine")]).await;
    assert_eq!(reply.content, "echo[2]: fine images=0");
}

#[tokio::test]
async fn crash_is_reported_and_session_resumes() {
    let s = start("").await;
    s.chat("c8", &[user("one")]).await;
    let crashed = s.chat("c8", &[user("one"), assistant("x"), user("CRASH")]).await;
    assert_eq!(crashed.errors, ["the agent process exited unexpectedly"]);

    let history = [user("one"), assistant("x"), user("CRASH"), assistant(""), user("three")];
    assert_eq!(s.chat("c8", &history).await.content, "echo[3]: three images=0");
    let logs = s.args_logs();
    assert_eq!(logs.len(), 2);
    assert!(logs[1].contains(&"--resume".to_string()), "{:?}", logs[1]);
}

#[tokio::test]
async fn idle_processes_stop_and_resume_from_disk() {
    let s = start(r#"idle_timeout = "0s""#).await;
    s.chat("c9", &[user("one")]).await;
    assert_eq!(s.sessions.live_count().await, 0);
    let reply = s.chat("c9", &[user("one"), assistant("x"), user("two")]).await;
    assert_eq!(reply.content, "echo[2]: two images=0");
    let logs = s.args_logs();
    assert_eq!(logs.len(), 2);
    assert!(logs[1].contains(&"--resume".to_string()));
}

#[tokio::test]
async fn max_live_evicts_least_recently_used_and_index_survives_restart() {
    let s = start("max_live = 1").await;
    s.chat("a", &[user("one")]).await;
    s.chat("b", &[user("one")]).await;
    assert_eq!(s.sessions.live_count().await, 1);
    // chat "a" was evicted but resumes with its history
    let reply = s.chat("a", &[user("one"), assistant("x"), user("two")]).await;
    assert_eq!(reply.content, "echo[2]: two images=0");

    // a new manager (proxy restart) loads the persisted index
    s.sessions.shutdown_all().await;
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(s.data.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(index["omp-test:a"]["forwarded"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delete_session_removes_state() {
    let s = start("").await;
    s.chat("gone", &[user("one")]).await;
    let resp: Value = s
        .client
        .delete(format!("{}/v1/sessions/gone", s.url))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["deleted"], 1);
    assert_eq!(s.sessions.live_count().await, 0);
    // history is gone: the same chat starts from scratch
    let reply = s.chat("gone", &[user("one"), assistant("x"), user("two")]).await;
    assert!(reply.content.starts_with("echo[1]: Earlier conversation"), "{}", reply.content);
}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test --test api`
Expected: compile errors (`tell_me_pi::api`, `session` not found).

**Step 3: Implement the session manager**

`src/session.rs`:

```rust
//! One omp process per chat: lookup, spawn/resume, history planning, idle eviction and the
//! persisted chat index.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast};

use crate::{
    config::{Config, Profile},
    history::{self, ChatMessage, ForwardedTurn, Plan},
    repo::{RepoTool, TOOL_NAME},
    rpc::{HostToolFn, OmpProcess, SpawnSpec, ToolOutcome},
};

/// Environment variables every omp child gets (when set), in addition to `omp.env_passthrough`.
const BASE_ENV: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "TMPDIR", "PI_CODING_AGENT_DIR", "PI_CONFIG_DIR"];
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const IDLE_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexEntry {
    session_file: Option<String>,
    forwarded: Vec<ForwardedTurn>,
    updated_at: u64,
}

pub struct Session {
    key: String,
    dir: PathBuf,
    profile: Profile,
    state: Mutex<State>,
}

struct State {
    proc: Option<Arc<OmpProcess>>,
    forwarded: Vec<ForwardedTurn>,
    session_file: Option<String>,
    busy: bool,
    generation: u64,
    last_used: Instant,
}

/// A running turn. Pass it back to [`SessionManager::end_turn`] when done.
pub struct Turn {
    session: Arc<Session>,
    proc: Arc<OmpProcess>,
    generation: u64,
    pub events: broadcast::Receiver<Value>,
    /// The prompt completed without an agent run (e.g. a slash command).
    pub local_only: bool,
}

pub struct SessionManager {
    cfg: Arc<Config>,
    repo: Arc<RepoTool>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    index: Mutex<HashMap<String, IndexEntry>>,
}

impl SessionManager {
    pub fn new(cfg: Arc<Config>, repo: Arc<RepoTool>) -> anyhow::Result<Arc<Self>> {
        let sessions_dir = cfg.sessions.sessions_dir();
        std::fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
        let index = match std::fs::read_to_string(cfg.sessions.index_path()) {
            Ok(text) => serde_json::from_str(&text).context("corrupt session index")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("reading session index"),
        };
        Ok(Arc::new(Self { cfg, repo, sessions: Mutex::default(), index: Mutex::new(index) }))
    }

    pub async fn begin_turn(&self, key: &str, profile: &Profile, messages: &[ChatMessage]) -> Result<Turn, TurnError> {
        let turns = history::user_turns(messages);
        let Some(last) = turns.last() else {
            return Err(TurnError::BadRequest("the request contains no user message".into()));
        };
        let session = self.get_or_load(key, profile).await;
        let mut st = session.state.lock().await;

        let alive = st.proc.as_ref().is_some_and(|p| p.is_alive());
        if !alive {
            st.proc = None;
            self.make_room(key).await?;
            let (proc, file) = self.spawn(&session, st.session_file.as_deref()).await?;
            st.proc = Some(proc);
            if file.is_some() {
                st.session_file = file;
            }
        }
        let proc = st.proc.clone().expect("process spawned above");

        if st.busy {
            // The user moved on (stop + new message, regenerate, …): cancel the running turn first.
            proc.request(json!({"type": "abort"})).await?;
            wait_until_idle(&proc).await?;
            st.busy = false;
        }

        let mut plan = history::plan(&st.forwarded, &turns);
        if let Plan::BranchAt(index) = plan {
            let resp = proc.request(json!({"type": "get_branch_messages"})).await?;
            let entries: Vec<(String, String)> = resp["data"]["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| Some((m["entryId"].as_str()?.to_string(), m["text"].as_str()?.to_string())))
                .collect();
            match history::find_entry(&st.forwarded, index, &entries) {
                Some(entry_id) => {
                    proc.request(json!({"type": "branch", "entryId": entry_id})).await?;
                    st.forwarded.truncate(index);
                }
                None => plan = Plan::Rebuild,
            }
        }
        let message = match plan {
            Plan::Prompt | Plan::BranchAt(_) => last.text.clone(),
            Plan::Rebuild => {
                if !st.forwarded.is_empty() {
                    proc.request(json!({"type": "new_session"})).await?;
                }
                st.forwarded = turns[..turns.len() - 1]
                    .iter()
                    .map(|t| ForwardedTurn { hash: t.hash.clone(), sent_text: None })
                    .collect();
                history::rebuild_prompt(messages, last)
            }
        };

        let events = proc.subscribe();
        let mut prompt = json!({"type": "prompt", "message": message});
        if !last.images.is_empty() {
            prompt["images"] = json!(last.images);
        }
        let ack = proc.request(prompt).await?;
        st.forwarded.push(ForwardedTurn { hash: last.hash.clone(), sent_text: Some(message) });
        st.generation += 1;
        st.busy = true;
        st.last_used = Instant::now();
        let generation = st.generation;
        drop(st);

        Ok(Turn {
            session: session.clone(),
            proc,
            generation,
            events,
            local_only: ack["data"]["agentInvoked"] == false,
        })
    }

    /// Marks the turn finished (aborting it first if `aborted`) and persists the chat state.
    pub async fn end_turn(&self, turn: Turn, aborted: bool) {
        let session = turn.session;
        let mut st = session.state.lock().await;
        if st.generation != turn.generation {
            return; // a newer turn took over
        }
        if aborted && turn.proc.is_alive() {
            let _ = turn.proc.request(json!({"type": "abort"})).await;
        }
        st.busy = false;
        st.last_used = Instant::now();
        // A prompt can be acknowledged and still fail before omp stores it (e.g. missing login).
        if turn.proc.is_alive()
            && let Some(sent) = st.forwarded.last().and_then(|t| t.sent_text.clone())
            && let Ok(resp) = turn.proc.request(json!({"type": "get_branch_messages"})).await
            && resp["data"]["messages"].as_array().and_then(|m| m.last()).map(|m| &m["text"]) != Some(&json!(sent))
        {
            st.forwarded.pop();
        }
        if let Ok(state) = turn.proc.request(json!({"type": "get_state"})).await
            && let Some(file) = state["data"]["sessionFile"].as_str()
        {
            st.session_file = Some(file.to_string());
        }
        if !turn.proc.is_alive() {
            st.proc = None;
        } else if self.cfg.sessions.idle_timeout.is_zero() {
            turn.proc.shutdown(SHUTDOWN_GRACE).await;
            st.proc = None;
        }
        let entry = IndexEntry {
            session_file: st.session_file.clone(),
            forwarded: st.forwarded.clone(),
            updated_at: unix_now(),
        };
        drop(st);
        let mut index = self.index.lock().await;
        index.insert(session.key.clone(), entry);
        if let Err(e) = self.save_index(&index).await {
            tracing::error!("saving session index failed: {e:#}");
        }
    }

    /// Stops idle processes and deletes chats older than the retention period.
    pub async fn maintain(&self) {
        let idle_timeout = self.cfg.sessions.idle_timeout;
        for session in self.all_sessions().await {
            let Ok(mut st) = session.state.try_lock() else { continue };
            if !st.busy
                && st.last_used.elapsed() >= idle_timeout
                && let Some(proc) = st.proc.take()
            {
                tracing::info!(key = %session.key, "stopping idle omp process");
                proc.shutdown(SHUTDOWN_GRACE).await;
            }
        }

        let cutoff = unix_now().saturating_sub(self.cfg.sessions.retention.as_secs());
        let expired: Vec<String> = {
            let index = self.index.lock().await;
            index.iter().filter(|(_, e)| e.updated_at < cutoff).map(|(k, _)| k.clone()).collect()
        };
        for key in expired {
            let live = self.sessions.lock().await.get(&key).cloned();
            if let Some(session) = live
                && session.state.try_lock().map(|st| st.proc.is_some()).unwrap_or(true)
            {
                continue;
            }
            tracing::info!(%key, "removing expired chat");
            self.remove(&key).await;
        }
    }

    /// Deletes every session of `chat_id` (all profiles). Returns how many were removed.
    pub async fn delete_chat(&self, chat_id: &str) -> usize {
        let suffix = format!(":{chat_id}");
        let mut keys: Vec<String> = self.index.lock().await.keys().filter(|k| k.ends_with(&suffix)).cloned().collect();
        for key in self.sessions.lock().await.keys() {
            if key.ends_with(&suffix) && !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        for key in &keys {
            self.remove(key).await;
        }
        keys.len()
    }

    pub async fn shutdown_all(&self) {
        for session in self.all_sessions().await {
            let proc = session.state.lock().await.proc.take();
            if let Some(proc) = proc {
                proc.shutdown(SHUTDOWN_GRACE).await;
            }
        }
    }

    pub async fn live_count(&self) -> usize {
        let mut n = 0;
        for session in self.all_sessions().await {
            if session.state.lock().await.proc.as_ref().is_some_and(|p| p.is_alive()) {
                n += 1;
            }
        }
        n
    }

    async fn remove(&self, key: &str) {
        let session = self.sessions.lock().await.remove(key);
        if let Some(session) = &session {
            let proc = session.state.lock().await.proc.take();
            if let Some(proc) = proc {
                proc.shutdown(SHUTDOWN_GRACE).await;
            }
        }
        let _ = tokio::fs::remove_dir_all(self.session_dir(key)).await;
        let mut index = self.index.lock().await;
        if index.remove(key).is_some()
            && let Err(e) = self.save_index(&index).await
        {
            tracing::error!("saving session index failed: {e:#}");
        }
    }

    async fn all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    async fn get_or_load(&self, key: &str, profile: &Profile) -> Arc<Session> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(key) {
            return s.clone();
        }
        let saved = self.index.lock().await.get(key).cloned().unwrap_or_default();
        let session = Arc::new(Session {
            key: key.to_string(),
            dir: self.session_dir(key),
            profile: profile.clone(),
            state: Mutex::new(State {
                proc: None,
                forwarded: saved.forwarded,
                session_file: saved.session_file,
                busy: false,
                generation: 0,
                last_used: Instant::now(),
            }),
        });
        sessions.insert(key.to_string(), session.clone());
        session
    }

    fn session_dir(&self, key: &str) -> PathBuf {
        let hash = hex::encode(Sha256::digest(key.as_bytes()));
        self.cfg.sessions.sessions_dir().join(&hash[..32])
    }

    /// Ensures a new process fits under `max_live`, stopping the longest-idle one if needed.
    async fn make_room(&self, own_key: &str) -> Result<(), TurnError> {
        let mut live = 0;
        let mut candidate: Option<(Instant, Arc<Session>)> = None;
        for session in self.all_sessions().await {
            if session.key == own_key {
                continue;
            }
            let Ok(st) = session.state.try_lock() else {
                live += 1; // busy starting or finishing a turn
                continue;
            };
            if !st.proc.as_ref().is_some_and(|p| p.is_alive()) {
                continue;
            }
            live += 1;
            if !st.busy && candidate.as_ref().is_none_or(|(t, _)| st.last_used < *t) {
                candidate = Some((st.last_used, session.clone()));
            }
        }
        if live < self.cfg.sessions.max_live {
            return Ok(());
        }
        let Some((_, victim)) = candidate else {
            return Err(TurnError::Unavailable("too many active conversations, try again later".into()));
        };
        let proc = victim.state.lock().await.proc.take();
        if let Some(proc) = proc {
            tracing::info!(key = %victim.key, "stopping omp process to make room");
            proc.shutdown(SHUTDOWN_GRACE).await;
        }
        Ok(())
    }

    async fn spawn(
        &self,
        session: &Session,
        resume: Option<&str>,
    ) -> anyhow::Result<(Arc<OmpProcess>, Option<String>)> {
        let workdir = session.dir.join("work");
        let omp_dir = session.dir.join("omp");
        tokio::fs::create_dir_all(&workdir).await?;
        tokio::fs::create_dir_all(&omp_dir).await?;

        let omp = &self.cfg.omp;
        let mut args: Vec<String> = vec![
            "--mode".into(),
            "rpc".into(),
            "--cwd".into(),
            path_arg(&workdir)?,
            "--session-dir".into(),
            path_arg(&omp_dir)?,
            "--model".into(),
            session.profile.model.clone(),
        ];
        if let Some(thinking) = &session.profile.thinking {
            args.extend(["--thinking".into(), thinking.clone()]);
        }
        args.extend(["--tools".into(), omp.tools.join(","), "--approval-mode".into(), "yolo".into()]);
        args.extend(omp.extra_args.iter().cloned());
        if let Some(file) = resume.filter(|f| Path::new(f).exists()) {
            args.extend(["--resume".into(), file.to_string()]);
        }

        let env: Vec<(String, String)> = BASE_ENV
            .iter()
            .copied()
            .chain(omp.env_passthrough.iter().map(String::as_str))
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect();

        let repo = self.repo.clone();
        let tool_workdir = workdir.clone();
        let handler: HostToolFn = Arc::new(move |call| {
            let repo = repo.clone();
            let workdir = tool_workdir.clone();
            Box::pin(async move {
                if call.tool_name != TOOL_NAME {
                    return ToolOutcome { text: format!("unknown host tool {}", call.tool_name), is_error: true };
                }
                let progress = call.progress.clone();
                let report = move |text: &str| progress.update(text);
                match repo.execute(&workdir, call.arguments, &report, &call.cancel).await {
                    Ok(text) => ToolOutcome { text, is_error: false },
                    Err(e) => ToolOutcome { text: format!("{e:#}"), is_error: true },
                }
            })
        });

        let spec = SpawnSpec { binary: omp.binary.clone(), args, cwd: workdir, env };
        let proc = OmpProcess::spawn(spec, Some(handler)).await?;
        proc.request(json!({"type": "set_host_tools", "tools": [self.repo.definition()]})).await?;
        let state = proc.request(json!({"type": "get_state"})).await?;
        let file = state["data"]["sessionFile"].as_str().map(String::from);
        tracing::info!(key = %session.key, resumed = resume.is_some(), "started omp process");
        Ok((proc, file))
    }

    async fn save_index(&self, index: &HashMap<String, IndexEntry>) -> anyhow::Result<()> {
        let path = self.cfg.sessions.index_path();
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(index)?).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

async fn wait_until_idle(proc: &OmpProcess) -> anyhow::Result<()> {
    let deadline = Instant::now() + IDLE_WAIT;
    loop {
        let state = proc.request(json!({"type": "get_state"})).await?;
        if state["data"]["isStreaming"] != true {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("the previous turn did not stop in time");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn path_arg(p: &Path) -> anyhow::Result<String> {
    p.to_str().map(String::from).context("path is not valid UTF-8")
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
```

Key behaviours:
- **Busy chat:** `abort`, then poll `get_state` until the old run stops, then prompt. This keeps
  the old run's `agent_end` out of the new stream. A superseded turn's `end_turn` is a no-op
  because its generation no longer matches.
- **After each turn:**
  - drop the last sent turn if omp didn't store it (acknowledged-then-failed prompt);
  - refresh `sessionFile`, which changes after `branch`/`new_session`;
  - persist `index.json` atomically.
- **Session directories** are named by the SHA-256 of the chat key, so a header value can't inject
  a path.
- **`make_room`** only `try_lock`s other sessions and skips its own, so it cannot deadlock.

**Step 4: Implement the HTTP API**

`src/api.rs`:

```rust
//! OpenAI-compatible HTTP API.

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::broadcast::error::RecvError, time::Instant};

use crate::{
    config::Config,
    history::ChatMessage,
    session::{SessionManager, Turn, TurnError},
    translate::{Out, Translator, Usage},
};

pub const CHAT_ID_HEADER: &str = "x-openwebui-chat-id";

pub struct AppState {
    pub cfg: Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub api_key: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/sessions/{chat_id}", delete(delete_session))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new().route("/healthz", get(|| async { "ok" })).merge(api).with_state(state)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    let kind = if status.is_server_error() { "server_error" } else { "invalid_request_error" };
    (status, Json(json!({"error": {"message": message.into(), "type": kind}}))).into_response()
}

impl IntoResponse for TurnError {
    fn into_response(self) -> Response {
        match self {
            TurnError::BadRequest(m) => error(StatusCode::BAD_REQUEST, m),
            TurnError::Unavailable(m) => error(StatusCode::SERVICE_UNAVAILABLE, m),
            TurnError::Internal(e) => {
                tracing::error!("turn failed: {e:#}");
                error(StatusCode::BAD_GATEWAY, format!("agent error: {e:#}"))
            }
        }
    }
}

async fn auth(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(key) = &app.api_key {
        let presented = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), key.as_bytes()) {
            return error(StatusCode::UNAUTHORIZED, "invalid API key");
        }
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn models(State(app): State<Arc<AppState>>) -> Json<Value> {
    let data: Vec<Value> = app
        .cfg
        .profiles
        .iter()
        .map(|p| json!({"id": p.name, "object": "model", "created": 0, "owned_by": "omp-proxy"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

async fn delete_session(State(app): State<Arc<AppState>>, Path(chat_id): Path<String>) -> Json<Value> {
    let removed = app.sessions.delete_chat(&chat_id).await;
    Json(json!({"deleted": removed}))
}

#[derive(Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

async fn chat_completions(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    let Some(profile) = app.cfg.profile(&req.model).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("unknown model {:?}", req.model));
    };
    let chat_id = headers
        .get(CHAT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .unwrap_or_else(|| fallback_chat_id(&req.messages));
    let key = format!("{}:{}", profile.name, chat_id);
    let turn = match app.sessions.begin_turn(&key, &profile, &req.messages).await {
        Ok(turn) => turn,
        Err(e) => return e.into_response(),
    };
    let run = TurnRun {
        sessions: app.sessions.clone(),
        turn: Some(turn),
        translator: Translator::new(app.cfg.omp.reasoning),
        deadline: Instant::now() + app.cfg.sessions.turn_timeout,
        buf: VecDeque::new(),
    };
    if req.stream { stream_response(run, req.model) } else { full_response(run, req.model).await }
}

/// Chats without the OpenWebUI header are keyed by their first user message.
fn fallback_chat_id(messages: &[ChatMessage]) -> String {
    let first = messages.iter().find(|m| m.role == "user").map(|m| m.text()).unwrap_or_default();
    format!("anon-{}", &hex::encode(Sha256::digest(first.as_bytes()))[..16])
}

/// Drives one turn: yields translated output until the turn is done, then ends it.
/// Dropping an unfinished run (client disconnect) aborts the turn.
struct TurnRun {
    sessions: Arc<SessionManager>,
    turn: Option<Turn>,
    translator: Translator,
    deadline: Instant,
    buf: VecDeque<Out>,
}

impl TurnRun {
    async fn next(&mut self) -> Option<Out> {
        loop {
            if let Some(out) = self.buf.pop_front() {
                if matches!(out, Out::Done | Out::Error(_)) {
                    self.buf.clear();
                    let aborted = matches!(out, Out::Error(_));
                    if let Some(turn) = self.turn.take() {
                        self.sessions.end_turn(turn, aborted).await;
                    }
                }
                return Some(out);
            }
            let turn = self.turn.as_mut()?;
            if turn.local_only {
                while let Ok(ev) = turn.events.try_recv() {
                    self.buf.extend(self.translator.on_event(&ev));
                }
                self.buf.push_back(Out::Done);
                continue;
            }
            tokio::select! {
                ev = turn.events.recv() => match ev {
                    Ok(ev) => self.buf.extend(self.translator.on_event(&ev)),
                    Err(RecvError::Lagged(n)) => self.buf.push_back(Out::Error(format!("lost {n} agent events"))),
                    Err(RecvError::Closed) => self.buf.push_back(Out::Error("the agent process ended".into())),
                },
                _ = tokio::time::sleep_until(self.deadline) => {
                    self.buf.push_back(Out::Error("the turn timed out".into()));
                }
            }
        }
    }

    fn usage(&self) -> Usage {
        self.translator.usage
    }
}

impl Drop for TurnRun {
    fn drop(&mut self) {
        if let Some(turn) = self.turn.take() {
            let sessions = self.sessions.clone();
            tokio::spawn(async move { sessions.end_turn(turn, true).await });
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn stream_response(mut run: TurnRun, model: String) -> Response {
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = unix_now();
    let chunk = move |delta: Value, finish: Option<&str>| {
        let v = json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        });
        Ok::<_, Infallible>(Event::default().data(v.to_string()))
    };
    let stream = async_stream::stream! {
        yield chunk(json!({"role": "assistant", "content": ""}), None);
        while let Some(out) = run.next().await {
            match out {
                Out::Content(text) => yield chunk(json!({"content": text}), None),
                Out::Reasoning(text) => yield chunk(json!({"reasoning_content": text}), None),
                Out::Done => {
                    yield chunk(json!({}), Some("stop"));
                    let usage = json!({"object": "chat.completion.chunk", "choices": [], "usage": run.usage()});
                    yield Ok(Event::default().data(usage.to_string()));
                }
                Out::Error(message) => {
                    let err = json!({"error": {"message": message, "type": "agent_error"}});
                    yield Ok(Event::default().data(err.to_string()));
                }
            }
        }
        yield Ok(Event::default().data("[DONE]"));
    };
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn full_response(mut run: TurnRun, model: String) -> Response {
    let (mut content, mut reasoning) = (String::new(), String::new());
    while let Some(out) = run.next().await {
        match out {
            Out::Content(t) => content.push_str(&t),
            Out::Reasoning(t) => reasoning.push_str(&t),
            Out::Done => {}
            Out::Error(e) => return error(StatusCode::BAD_GATEWAY, e),
        }
    }
    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    Json(json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": unix_now(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": run.usage(),
    }))
    .into_response()
}
```

Then set `src/lib.rs` to its final content:

```rust
pub mod api;
pub mod config;
pub mod history;
pub mod repo;
pub mod rpc;
pub mod session;
pub mod translate;
```

**Step 5: Run the tests to verify they pass**

Run: `cargo test`
Expected: all suites PASS (18 unit tests, api 12, repo_tool 2, rpc_process 4). `tests/api.rs`
should finish in about 1 s; if it takes 120 s, a request is waiting on a dead process (see Task 5).

**Step 6: Commit**

```bash
git add src/lib.rs src/session.rs src/api.rs tests/api.rs
git commit -m "feat: add per-chat omp session manager and OpenAI-compatible API"
```

### Task 10: Binary entry point

Reads the config and secrets as root, drops to the `omp` user (and marks the process
non-dumpable) *before* starting the tokio runtime, runs the server with periodic maintenance, and
shuts down gracefully on SIGTERM or Ctrl-C.

**Files:**
- Modify: `src/main.rs`

**Step 1: Write `src/main.rs`**

```rust
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use tell_me_pi::{
    api::{self, AppState},
    config::Config,
    repo::RepoTool,
    session::SessionManager,
};

const DEFAULT_CONFIG: &str = "/etc/omp-proxy/proxy.toml";
const DEFAULT_USER: &str = "omp";
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config_path = std::env::var_os("OMP_PROXY_CONFIG").map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from);
    let cfg = Arc::new(Config::load(&config_path)?);
    // Secrets are read while still privileged; afterwards the files are unreadable.
    let api_key = read_secret(cfg.server.api_key_file.as_deref())?;
    let git_token = read_secret(cfg.git.token_file.as_deref())?;
    if api_key.is_none() {
        tracing::warn!("no server.api_key_file configured: the API is unauthenticated");
    }
    drop_privileges(&std::env::var("OMP_PROXY_USER").unwrap_or_else(|_| DEFAULT_USER.into()))?;

    tokio::runtime::Runtime::new()?.block_on(serve(cfg, api_key, git_token))
}

async fn serve(cfg: Arc<Config>, api_key: Option<String>, git_token: Option<String>) -> anyhow::Result<()> {
    let token = git_token.as_deref().map(|t| (cfg.git.token_username.as_str(), t));
    let repo = Arc::new(RepoTool::new(cfg.git.repos.clone(), cfg.sessions.mirrors_dir(), token));
    let sessions = SessionManager::new(cfg.clone(), repo)?;

    let maintenance = sessions.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            tick.tick().await;
            maintenance.maintain().await;
        }
    });

    let app = api::router(Arc::new(AppState { cfg: cfg.clone(), sessions: sessions.clone(), api_key }));
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.listen))?;
    tracing::info!("listening on {}", cfg.server.listen);
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    sessions.shutdown_all().await;
    Ok(())
}

fn read_secret(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let value = std::fs::read_to_string(path).with_context(|| format!("reading secret {}", path.display()))?;
    let value = value.trim().to_string();
    anyhow::ensure!(!value.is_empty(), "secret {} is empty", path.display());
    Ok(Some(value))
}

/// Switches from root to `user` (when started as root) and marks the process non-dumpable,
/// so omp children running as the same user cannot read our memory or `/proc/<pid>/environ`.
fn drop_privileges(user: &str) -> anyhow::Result<()> {
    use nix::unistd::{Uid, User, setgid, setgroups, setuid};
    if Uid::effective().is_root() {
        let u = User::from_name(user)?.with_context(|| format!("user {user:?} does not exist"))?;
        setgroups(&[u.gid]).context("setgroups")?;
        setgid(u.gid).context("setgid")?;
        setuid(u.uid).context("setuid")?;
        anyhow::ensure!(setuid(Uid::from_raw(0)).is_err(), "privilege drop failed");
        tracing::info!("running as user {user}");
    }
    nix::sys::prctl::set_dumpable(false).context("prctl(PR_SET_DUMPABLE)")?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
```

**Step 2: Smoke test locally**

Run:
```bash
d=$(mktemp -d); printf k > "$d/key"
cat > "$d/proxy.toml" <<EOF
[server]
listen = "127.0.0.1:18080"
api_key_file = "$d/key"
[sessions]
data_dir = "$d/data"
[omp]
binary = "$PWD/target/debug/fake-omp"
[[profile]]
name = "omp-test"
model = "fake/model"
EOF
cargo build && OMP_PROXY_CONFIG="$d/proxy.toml" ./target/debug/omp-proxy & pid=$!; sleep 2
curl -s -H 'Authorization: Bearer k' localhost:18080/v1/models
curl -sN -H 'Authorization: Bearer k' -H 'X-OpenWebUI-Chat-Id: t1' -H 'Content-Type: application/json' \
  localhost:18080/v1/chat/completions \
  -d '{"model":"omp-test","stream":true,"messages":[{"role":"user","content":"hi"}]}'
kill $pid
```
Expected:
- the models list contains `omp-test`;
- the stream has chunks with `reasoning_content` `hmm` and content `echo[1]: hi images=0`, then
  `finish_reason: "stop"`, a usage chunk, and `data: [DONE]`;
- the log ends with `shutting down`.

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add omp-proxy entry point with privilege drop"
```

### Task 11: `repo-checkout` skill

**Files:**
- Create: `skills/repo-checkout/SKILL.md`

**Step 1: Write the skill**

````markdown
---
name: repo-checkout
description: Check out a repository at a specific release, tag or branch with the `repo` tool before answering questions about code. Use when a question concerns source code, a release, a version, or differences between versions.
---

# Checking out repositories

You start in an empty working directory. Code is only available after you check it out with the
`repo` tool. The tool is read-only: it can list, fetch and check out, never push or modify.

## When to check out

- Check out code only when the question is about source code, configuration, behaviour of a
  specific version, or a change between versions.
- Do not check out anything for general questions that do not need the code.
- Reuse checkouts that already exist in the working directory (`glob` for `*@*`).

## Picking the right ref

1. If the repository is unclear, call `repo` with `{"action": "list"}` and pick the matching one,
   or ask the user when several could match.
2. Call `{"action": "refs", "repo": "<name>"}`. Tags are listed newest version first.
3. Map the user's wording to a ref:
   - "release 1.4" / "version 1.4" → the newest tag matching `v1.4.*` (or `1.4.*`).
   - "v1.4.2" → that exact tag.
   - "latest release" → the first tag in the list.
   - "current", "main", "development" or no version given → the default branch (`main` or
     `master`).
4. If nothing matches, say so and list the closest refs instead of guessing.

## Checking out and reading

- `{"action": "checkout", "repo": "<name>", "ref": "<ref>"}` creates `./<name>@<ref>/`
  (a `/` in the ref becomes `_`).
- Then use `grep`, `glob` and `read` with paths under that directory.
- To compare versions, check out both refs and inspect the same paths in both directories.

## Answering

- State which repository and ref your answer is based on.
- Cite files as `path/to/file.rs:123` relative to the repository root (without the
  `<name>@<ref>/` prefix) and quote the relevant lines.
````

**Step 2: Commit**

```bash
git add skills
git commit -m "feat: add repo-checkout skill"
```

### Task 12: Docker, Compose and example config

**Files:**
- Create: `Dockerfile`, `.dockerignore`, `docker-compose.yml`, `.env.example`, `proxy.example.toml`

**Step 1: `proxy.example.toml`**

```toml
# omp-proxy configuration. Copy to proxy.toml and adjust.

[server]
listen = "0.0.0.0:8080"
# OpenWebUI must send this value as its API key (Bearer token).
api_key_file = "/run/secrets/proxy_api_key"

[sessions]
data_dir = "/data"      # sessions/, mirrors/, index.json
idle_timeout = "15m"    # stop idle omp processes; "0s" = one process per turn
max_live = 8            # concurrently running omp processes
turn_timeout = "10m"    # abort a single answer after this long
retention = "30d"       # delete chats not used for this long

[omp]
binary = "/usr/local/bin/omp"
# Read-only tool set. bash, eval, edit, write, ast_edit, task and debug are rejected.
# The `repo` host tool is always added.
tools = ["read", "grep", "glob", "todo"]
extra_args = []
# Environment variables passed to omp when set. Kept for reference: the Codex subscription
# login is stored in the data volume instead (see README).
env_passthrough = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENAI_CODEX_OAUTH_TOKEN"]
reasoning = "field"     # field (reasoning_content) | think_tags | off

# Each profile is shown as a model in OpenWebUI.
[[profile]]
name = "omp-codex"
model = "openai-codex/gpt-5.5"
thinking = "medium"

[git]
# Use a read-only token (GitHub fine-grained PAT with "Contents: read").
token_file = "/run/secrets/git_token"
token_username = "x-access-token"   # GitLab: "oauth2"

[[git.repo]]
name = "backend"
url = "https://github.com/acme/backend.git"
description = "Backend service"
```

**Step 2: `Dockerfile`**

```dockerfile
# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin omp-proxy

FROM debian:bookworm-slim
ARG OMP_VERSION=v18.2.3
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git tini \
    && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH:-amd64}" in \
        amd64) arch=x64 ;; \
        arm64) arch=arm64 ;; \
        *) echo "unsupported architecture ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /usr/local/bin/omp \
        "https://github.com/can1357/oh-my-pi/releases/download/${OMP_VERSION}/omp-linux-${arch}" \
    && chmod 0755 /usr/local/bin/omp
# HOME lives on the data volume: omp keeps its login (agent.db), caches and settings there.
RUN useradd --uid 10001 --home-dir /data/home --no-create-home --shell /usr/sbin/nologin omp \
    && mkdir -p /data/home/.omp/agent/skills /data/sessions /data/mirrors /etc/omp-proxy \
    && chown -R omp:omp /data
COPY --from=build /src/target/release/omp-proxy /usr/local/bin/omp-proxy
ENV HOME=/data/home \
    OMP_PROXY_CONFIG=/etc/omp-proxy/proxy.toml \
    OMP_PROXY_USER=omp
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
# Starts as root to read the secrets, then drops to the omp user.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/omp-proxy"]
```

**Step 3: `.dockerignore`**

```text
target
.git
secrets
data
docs
tests
*.md
.env
```

**Step 4: `docker-compose.yml`**

```yaml
name: tell-me-pi

services:
  omp-proxy:
    build: .
    image: tell-me-pi/omp-proxy:latest
    restart: unless-stopped
    read_only: true
    tmpfs:
      - /tmp
    cap_drop: [ALL]
    cap_add: [SETUID, SETGID]   # only needed to switch from root to the omp user
    security_opt:
      - no-new-privileges:true
    pids_limit: 512
    mem_limit: 4g
    cpus: 2
    environment:
      # Optional provider keys (unused with the Codex subscription login, kept for reference).
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY:-}
      OPENAI_API_KEY: ${OPENAI_API_KEY:-}
      OPENAI_CODEX_OAUTH_TOKEN: ${OPENAI_CODEX_OAUTH_TOKEN:-}
    secrets:
      - git_token
      - proxy_api_key
    volumes:
      - omp-data:/data
      - ./proxy.toml:/etc/omp-proxy/proxy.toml:ro
      - ./skills:/data/home/.omp/agent/skills:ro
    expose:
      - "8080"

  open-webui:
    image: ghcr.io/open-webui/open-webui:${OPEN_WEBUI_VERSION:-main}
    restart: unless-stopped
    depends_on:
      omp-proxy:
        condition: service_healthy
    ports:
      - "${OPEN_WEBUI_PORT:-3000}:8080"
    environment:
      WEBUI_SECRET_KEY: ${WEBUI_SECRET_KEY:?set WEBUI_SECRET_KEY in .env}
      ENABLE_OLLAMA_API: "false"
      # First connection: the omp proxy. Second: a regular LLM for titles, tags and follow-ups.
      OPENAI_API_BASE_URLS: "http://omp-proxy:8080/v1;${TASK_API_BASE_URL:?set TASK_API_BASE_URL in .env}"
      OPENAI_API_KEYS: "${PROXY_API_KEY:?set PROXY_API_KEY in .env};${TASK_API_KEY:?set TASK_API_KEY in .env}"
      TASK_MODEL_EXTERNAL: ${TASK_MODEL:?set TASK_MODEL in .env}
      # Sends X-OpenWebUI-Chat-Id, which the proxy uses to keep one agent session per chat.
      ENABLE_FORWARD_USER_INFO_HEADERS: "true"
      # Agent turns can be long.
      AIOHTTP_CLIENT_TIMEOUT: "900"
    volumes:
      - open-webui-data:/app/backend/data

secrets:
  git_token:
    file: ./secrets/git_token
  proxy_api_key:
    file: ./secrets/proxy_api_key

volumes:
  omp-data:
  open-webui-data:
```

**Step 5: `.env.example`**

```bash
# Copy to .env and fill in.

# Must equal the content of secrets/proxy_api_key.
PROXY_API_KEY=change-me
WEBUI_SECRET_KEY=change-me-too
OPEN_WEBUI_PORT=3000
# OPEN_WEBUI_VERSION=main

# Regular OpenAI-compatible LLM for OpenWebUI's background tasks (titles, tags, follow-ups).
TASK_API_BASE_URL=https://api.openai.com/v1
TASK_API_KEY=sk-...
TASK_MODEL=gpt-5-mini

# Optional, unused with the Codex subscription login.
# ANTHROPIC_API_KEY=
# OPENAI_API_KEY=
# OPENAI_CODEX_OAUTH_TOKEN=
```

**Step 6: Validate**

Run:
```bash
cp proxy.example.toml proxy.toml && cp .env.example .env
mkdir -p secrets && printf x > secrets/git_token && printf change-me > secrets/proxy_api_key
docker compose config -q && echo OK
docker compose build omp-proxy
docker run --rm --entrypoint omp tell-me-pi/omp-proxy:latest --version
```
Expected:
- `OK`, then a successful build;
- `omp/18.2.3`.

Then check the privilege drop:
```bash
docker compose up -d omp-proxy && sleep 3
docker compose exec omp-proxy sh -c 'ps -o user,comm; cat /run/secrets/git_token' 
```
Expected:
- the logs show `running as user omp` and the health check turns `healthy`;
- `ps` shows `omp-proxy` running as `omp`;
- `cat` fails with *Permission denied* if you ran the `chown 0:0 / chmod 600` step from the
  README. Otherwise the secret is readable; fix the file ownership.

Run `docker compose down` afterwards.

**Step 7: Commit**

```bash
git add Dockerfile .dockerignore docker-compose.yml .env.example proxy.example.toml
git commit -m "feat: add Docker image and compose deployment with OpenWebUI"
```

### Task 13: README and design doc update

**Files:**
- Create: `README.md`
- Modify: `docs/plans/2026-09-17-omp-openwebui-proxy-design.md`

**Step 1: Write `README.md`**

````markdown
# tell_me_pi — omp agent behind OpenWebUI

`omp-proxy` is an OpenAI-compatible server (`/v1/models`, `/v1/chat/completions`) that runs the
[oh-my-pi](https://github.com/can1357/oh-my-pi) coding agent (`omp --mode rpc`) for each
[OpenWebUI](https://github.com/open-webui/open-webui) chat. The agent is read-only. It can check out
releases of the configured repositories with its `repo` tool and answer questions about them.

Design: [`docs/plans/2026-09-17-omp-openwebui-proxy-design.md`](docs/plans/2026-09-17-omp-openwebui-proxy-design.md)

## How it works

- Each OpenWebUI chat (`X-OpenWebUI-Chat-Id`) gets its own omp process and an empty working
  directory.
- Idle processes are stopped and resumed from their session file on the next message.
- The `repo` tool (`list`, `refs`, `checkout`) runs inside the proxy:
  - it keeps shared, blobless mirrors in `/data/mirrors`;
  - it checks refs out as `./<repo>@<ref>/` worktrees;
  - the git token never reaches the agent;
  - there is no push.
- The agent's only other tools are `read`, `grep`, `glob` and `todo`, so it cannot run
  commands or write files.
- Skills come from `./skills` (mounted read-only). `skills/repo-checkout` teaches the agent how to
  pick refs.

## Deploy

1. Configure:

   ```sh
   cp proxy.example.toml proxy.toml     # profiles, repositories
   cp .env.example .env                 # OpenWebUI + task model settings
   mkdir -p secrets
   printf '%s' "<read-only git token>" > secrets/git_token
   printf '%s' "<random key>"          > secrets/proxy_api_key   # same value as PROXY_API_KEY in .env
   sudo chown 0:0 secrets/* && sudo chmod 600 secrets/*          # only root in the container can read them
   ```

   Use a read-only token, e.g. a GitHub fine-grained PAT with only *Contents: read* on the
   configured repositories.

2. Build, then log in to the OpenAI Codex subscription once. The login is stored in the `omp-data`
   volume and refreshed by omp:

   ```sh
   docker compose build
   docker compose run --rm -it --user omp --entrypoint omp omp-proxy
   # inside omp: /login  → choose OpenAI Codex (ChatGPT) → follow the browser flow → /exit
   ```

3. Start:

   ```sh
   docker compose up -d
   ```

   OpenWebUI listens on `http://localhost:3000`. Put a TLS reverse proxy in front of it for cloud
   use. The proxy port is not published.

4. In OpenWebUI, pick a model named after a profile (e.g. `omp-codex`) and ask away.

The proxy does not handle OpenWebUI's background tasks (titles, tags, follow-ups): `TASK_MODEL`
must be a model served by the second connection (`TASK_API_BASE_URL`).

## Operations

- Logs: `docker compose logs -f omp-proxy`
- Delete a chat's agent state: `curl -X DELETE -H "Authorization: Bearer $PROXY_API_KEY" http://omp-proxy:8080/v1/sessions/<chat-id>`
  (from inside the compose network).
- Chats unused for `sessions.retention` are removed automatically.
- Update omp: change `OMP_VERSION` in the `Dockerfile` and run `docker compose build`.

## Security notes

- Read-only enforcement:
  - the tool list rejects `bash`, `eval`, `edit`, `write`, `ast_edit`, `task` and `debug`;
  - approval prompts are disabled (`--approval-mode yolo`) so no turn can stall.
- The proxy reads secrets as root, then switches to the `omp` user and marks itself non-dumpable.
- The container runs with:
  - a read-only root filesystem;
  - no capabilities except `SETUID`/`SETGID`;
  - `no-new-privileges` and resource limits.
- Known limitation: omp's `read` tool can read anything the `omp` user can read, including other
  chats' working directories and `/data/home/.omp/agent/agent.db` (the Codex login). This is fine
  for a single trusted user; revisit before sharing the instance.
- A ChatGPT Pro subscription is personal. Serving it to several users may violate OpenAI's terms and
  will hit per-account rate limits.

## Development

```sh
cargo test            # unit tests + integration tests against the fake omp (src/bin/fake_omp.rs)
cargo clippy --all-targets
```

`OMP_PROXY_CONFIG=./proxy.toml cargo run --bin omp-proxy` runs the proxy locally. Set
`sessions.data_dir` to a writable path and `omp.binary` to your local `omp` first.
````

**Step 2: Update the design doc** with the deviations listed at the top of this plan:
- tool list `read, grep, glob, todo`;
- `abort` + wait instead of `abort_and_prompt`;
- `HOME=/data/home` with skills at `/data/home/.omp/agent/skills`;
- the mutex instead of `flock`;
- the "drop unstored turn" reconciliation in `end_turn`.

**Step 3: Commit**

```bash
git add README.md docs/plans
git commit -m "docs: add README and align design doc with implementation"
```

### Task 14: Manual end-to-end verification with the real omp and OpenWebUI

No code changes; this checks what the fake cannot.

1. Fill `proxy.toml` with a real repository and `secrets/git_token` with a read-only token. Set
   `.env`.
2. `docker compose build`, then log in once:
   `docker compose run --rm -it --user omp --entrypoint omp omp-proxy` → `/login` → OpenAI Codex →
   `/exit`.
3. `docker compose up -d` and open OpenWebUI. Select `omp-codex`, then check:
   - **Non-code question.** "What is a mutex?" gets an answer with no `repo` tool call, and
     `docker compose exec omp-proxy ls /data/sessions/*/work` shows an empty directory.
   - **Release question.** "In release 1.4 of backend, where is the config loaded?"
     - the thinking panel shows `🔧 repo refs backend`, then `🔧 repo checkout backend v1.4.x`;
     - the answer cites files;
     - a `✓ repo checkout …` details block appears.
   - **Follow-up.** "And in the latest release?" is answered in the same session: the proxy logs
     show no new `started omp process`.
   - **Regenerate** the last answer: no duplicate context (the answer doesn't mention the question
     twice).
   - **Edit** the first message: the answer is based only on the edited question.
   - **Stop** a long answer, then send a new message: the new answer arrives.
   - **Idle resume.** Wait past `idle_timeout` (or set it to `1m`) and ask a follow-up:
     - the log shows `stopping idle omp process`, then `started omp process … resumed=true`;
     - the answer still knows the earlier context.
   - **Two chats in parallel** both stream.
   - **Push is impossible.** Ask the agent to run `git push`. It has no tool for that, and
     `repo` rejects unknown actions.
   - **Titles and tags** are generated by `TASK_MODEL` (check the proxy logs: no requests for
     them).
4. Note anything that fails as a bug, and debug it with superpowers:systematic-debugging before
   changing code.
