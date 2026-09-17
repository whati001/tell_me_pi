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
                        // A separate task, so a client disconnect cannot cancel it half-way.
                        let sessions = self.sessions.clone();
                        let _ = tokio::spawn(async move { sessions.end_turn(turn, aborted).await }).await;
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
                // superseded by a newer request for the same chat
                _ = turn.cancelled.cancelled() => self.buf.push_back(Out::Done),
            }
        }
    }

    fn usage(&self) -> Usage {
        self.translator.usage
    }
}

impl Drop for TurnRun {
    fn drop(&mut self) {
        if let Some(turn) = self.turn.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let sessions = self.sessions.clone();
            runtime.spawn(async move { sessions.end_turn(turn, true).await });
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn stream_response(mut run: TurnRun, model: String) -> Response {
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = unix_now();
    let base = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model});
    let with_base = move |fields: Value| {
        let mut v = base.clone();
        for (k, field) in fields.as_object().into_iter().flatten() {
            v[k] = field.clone();
        }
        Ok::<_, Infallible>(Event::default().data(v.to_string()))
    };
    let chunk = {
        let with_base = with_base.clone();
        move |delta: Value, finish: Option<&str>| {
            with_base(json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}))
        }
    };
    let stream = async_stream::stream! {
        yield chunk(json!({"role": "assistant", "content": ""}), None);
        while let Some(out) = run.next().await {
            match out {
                Out::Content(text) => yield chunk(json!({"content": text}), None),
                Out::Reasoning(text) => yield chunk(json!({"reasoning_content": text}), None),
                Out::Done => {
                    yield chunk(json!({}), Some("stop"));
                    yield with_base(json!({"choices": [], "usage": run.usage()}));
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
