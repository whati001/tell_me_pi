//! End-to-end tests: HTTP API → session manager → fake omp.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tell_me_where::{
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

        [[profile]]
        name = "omp-leaky"
        model = "fake/leaky"

        [[profile]]
        name = "omp-notools"
        model = "fake/notools"
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
            assert!(v["id"].is_string() && v["created"].is_u64() && v["model"].is_string(), "{v}");
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
    assert!(first.errors.is_empty(), "{:?}", first.errors);
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

#[tokio::test]
async fn late_failure_after_rebuild_keeps_the_transcript() {
    let s = start("").await;
    let failed = s.chat("c11", &[user("first"), assistant("answer"), user("FAIL_LATE")]).await;
    assert_eq!(failed.errors, ["No API key found for fake."]);
    // omp stored nothing, so the earlier conversation must be sent again
    let reply = s.chat("c11", &[user("first"), assistant("answer"), user("second")]).await;
    assert!(reply.content.starts_with("echo[1]: Earlier conversation"), "{}", reply.content);
}

#[tokio::test]
async fn delete_during_turn_leaves_no_index_entry() {
    let s = Arc::new(start("").await);
    s.chat("busy-del", &[user("one")]).await;
    let slow = {
        let s = s.clone();
        tokio::spawn(async move { s.chat("busy-del", &[user("one"), assistant("x"), user("SLOW")]).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let resp = s.client.delete(format!("{}/v1/sessions/busy-del", s.url)).bearer_auth("secret").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    slow.await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(s.data.path().join("index.json")).unwrap()).unwrap();
    assert!(index.get("omp-test:busy-del").is_none(), "{index}");
}

#[tokio::test]
async fn delete_matches_chat_ids_exactly() {
    let s = start("").await;
    s.chat("x:y", &[user("one")]).await;
    let resp: Value = s
        .client
        .delete(format!("{}/v1/sessions/y", s.url))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["deleted"], 0);
}

#[tokio::test]
async fn corrupt_index_is_set_aside() {
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("index.json"), "{not json").unwrap();
    let cfg = Config::from_toml(&format!(
        "[sessions]\ndata_dir = \"{}\"\n[[profile]]\nname = \"a\"\nmodel = \"m\"\n",
        data.path().display()
    ))
    .unwrap();
    let cfg = Arc::new(cfg);
    let repo = Arc::new(RepoTool::new(Vec::new(), cfg.sessions.mirrors_dir(), None));
    SessionManager::new(cfg, repo).unwrap();
    assert!(!data.path().join("index.json").exists());
    assert_eq!(std::fs::read_to_string(data.path().join("index.json.corrupt")).unwrap(), "{not json");
}

#[tokio::test]
async fn tools_outside_the_read_only_set_are_refused() {
    let s = start("").await;
    let resp = s
        .client
        .post(format!("{}/v1/chat/completions", s.url))
        .bearer_auth("secret")
        .header("X-OpenWebUI-Chat-Id", "leaky")
        .json(&json!({"model": "omp-leaky", "messages": [user("hi")]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("read-only set") && message.contains("bash"), "{message}");
    assert_eq!(s.sessions.live_count().await, 0);
}

#[tokio::test]
async fn omp_without_tool_report_is_refused() {
    let s = start("").await;
    let resp = s
        .client
        .post(format!("{}/v1/chat/completions", s.url))
        .bearer_auth("secret")
        .header("X-OpenWebUI-Chat-Id", "notools")
        .json(&json!({"model": "omp-notools", "messages": [user("hi")]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("did not report its active tools"), "{message}");
    assert_eq!(s.sessions.live_count().await, 0);
}
