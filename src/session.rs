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
