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
                self.close_think(&mut out);
                out.push(Out::Error(err.to_string()));
            }
            t if t == PROCESS_EXIT_EVENT => {
                self.close_think(&mut out);
                out.push(Out::Error("the agent process exited unexpectedly".into()));
            }
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
    // Neither part may close the surrounding HTML block or code fence.
    let summary = summary.replace('<', "&lt;").replace('>', "&gt;").replace(['\r', '\n'], " ");
    let text = truncate(&text, MAX_TOOL_OUTPUT).replace("```", "ˋˋˋ").replace("</details", "&lt;/details");
    format!("\n<details>\n<summary>{mark} {summary}</summary>\n\n```\n{text}\n```\n</details>\n\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

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

    #[test]
    fn tool_output_cannot_break_out_of_details() {
        let mut t = Translator::new(ReasoningMode::Off);
        let start = json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "grep",
                           "args": {"pattern": "a<b>\n</summary>", "path": "src"}});
        t.on_event(&start);
        let end = json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "grep", "isError": false,
                         "result": {"content": [{"type": "text", "text": "x</details><img src=y>\n```\nz"}]}});
        let Out::Content(html) = &t.on_event(&end)[0] else { panic!() };
        assert_eq!(html.matches("</details").count(), 1, "{html}");
        assert!(html.contains("x&lt;/details><img src=y>"), "{html}");
        assert_eq!(html.matches("```").count(), 2, "{html}");
        assert!(html.contains("<summary>✓ grep a&lt;b&gt; &lt;/summary&gt; in src</summary>"), "{html}");
    }

    #[test]
    fn errors_close_an_open_think_block() {
        let fail = json!({"type": "response", "command": "prompt", "success": false, "error": "boom"});
        let exit = json!({"type": PROCESS_EXIT_EVENT});
        for ev in [fail, exit] {
            let mut t = Translator::new(ReasoningMode::ThinkTags);
            t.on_event(&delta("thinking_delta", "hm"));
            let out = t.on_event(&ev);
            assert_eq!(out[0], Out::Content("\n</think>\n\n".into()), "{ev}");
            assert!(matches!(out[1], Out::Error(_)), "{ev}");
            assert_eq!(out.len(), 2);
        }
    }
}
