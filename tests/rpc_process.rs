use std::{path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tell_me_where::rpc::{HostToolFn, OmpProcess, PROCESS_EXIT_EVENT, SpawnSpec, ToolOutcome};
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

#[tokio::test]
async fn panicking_host_tool_reports_an_error_result() {
    let dir = tempfile::tempdir().unwrap();
    let handler: HostToolFn = Arc::new(|_call| Box::pin(async move { panic!("handler exploded") }));
    let proc = OmpProcess::spawn(spec(&dir), Some(handler)).await.unwrap();
    let mut rx = proc.subscribe();
    proc.request(json!({"type": "prompt", "message": "USE_TOOL"})).await.unwrap();
    let events = collect_until_end(&mut rx).await;
    assert_eq!(text_of(&events), "tool said: host tool failed unexpectedly");
    let end = events.iter().find(|e| e["type"] == "tool_execution_end").unwrap();
    assert_eq!(end["isError"], true);
}

#[tokio::test]
async fn corrupt_stream_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let proc = OmpProcess::spawn(spec(&dir), None).await.unwrap();
    let mut rx = proc.subscribe();
    proc.request(json!({"type": "prompt", "message": "CORRUPT"})).await.unwrap();
    let events = collect_until_end(&mut rx).await;
    assert_eq!(events.last().unwrap()["type"], PROCESS_EXIT_EVENT);
    assert!(!events.iter().any(|e| e["type"] == "agent_start"), "{events:?}");
    assert!(!proc.is_alive());
    let err = tokio::time::timeout(Duration::from_secs(5), proc.request(json!({"type": "get_state"})))
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("not running"), "{err}");
}
