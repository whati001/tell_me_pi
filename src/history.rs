//! OpenAI request messages → user turns, and deciding how an incoming history maps onto the
//! omp session (normal follow-up, regenerate/edit via `branch`, or rebuild).

use std::sync::LazyLock;

use regex::Regex;
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

/// Appended to a turn's text for each message with image URLs that could not be turned into omp images
/// (only `data:image/…;base64,` URLs can be).
const IMAGE_NOT_LOADED: &str = "(An attached image could not be loaded.)";

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
            if text.trim().is_empty() && !images.is_empty() {
                text = "(see attached image)".into();
            }
            if images.len() < urls.len() {
                if !text.trim().is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(IMAGE_NOT_LOADED);
            }
            if text.trim().is_empty() {
                text = "(empty message)".into();
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
///
/// Returns `None` if the turn cannot be branched to, including when it is the turn that carried a rebuild
/// transcript (the previous turn was never sent on its own): branching there would drop that context, so the
/// caller must rebuild instead.
pub fn find_entry(known: &[ForwardedTurn], index: usize, entries: &[(String, String)]) -> Option<String> {
    known.get(index)?.sent_text.as_ref()?;
    if index > 0 && known[index - 1].sent_text.is_none() {
        return None;
    }
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
    static DETAILS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<details.*?</details>").expect("valid regex"));
    let last_user = messages.iter().rposition(|m| m.role == "user").unwrap_or(0);
    let mut transcript = String::new();
    for m in &messages[..last_user] {
        let who = match m.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            _ => continue,
        };
        let raw = m.text();
        let text = DETAILS.replace_all(&raw, "");
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
        assert_eq!(turns[0].text, format!("what is this\n\n{IMAGE_NOT_LOADED}"));
        assert_eq!(turns[0].images, vec![json!({"type": "image", "mimeType": "image/png", "data": "AAAA"})]);

        let empty = user_turns(&[ChatMessage { role: "user".into(), content: None }]);
        assert_eq!(empty[0].text, "(empty message)");
    }

    #[test]
    fn notes_images_that_could_not_be_loaded() {
        let image = |url: &str| json!({"type": "image_url", "image_url": {"url": url}});
        let user = |content: Value| -> ChatMessage {
            serde_json::from_value(json!({"role": "user", "content": content})).unwrap()
        };
        let note = "(An attached image could not be loaded.)";

        let t = user_turns(&[user(json!([{"type": "text", "text": "look"}, image("https://example.com/x.png")]))]);
        assert_eq!(t[0].text, format!("look\n\n{note}"));
        assert!(t[0].images.is_empty());

        let t = user_turns(&[user(json!([image("https://example.com/x.png")]))]);
        assert_eq!(t[0].text, note);

        let t = user_turns(&[user(json!([image("data:image/png;base64,AAAA")]))]);
        assert_eq!(t[0].text, "(see attached image)");

        let t = user_turns(&[user(json!([image("data:image/png;base64,AAAA"), image("http://x/y.png")]))]);
        assert_eq!(t[0].text, format!("(see attached image)\n\n{note}"));
        assert_eq!(t[0].images.len(), 1);
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
        let fwd =
            |hash: &str, sent: Option<&str>| ForwardedTurn { hash: hash.into(), sent_text: sent.map(String::from) };
        let entries = vec![
            ("e0".to_string(), "transcript…".to_string()),
            ("e1".to_string(), "again".to_string()),
            ("e2".to_string(), "again".to_string()),
        ];

        // after a rebuild: turn 0 only reached omp inside the transcript sent with turn 1
        let rebuilt = vec![fwd("1", None), fwd("2", Some("transcript…")), fwd("3", Some("again"))];
        assert_eq!(find_entry(&rebuilt, 2, &entries), Some("e1".into()));
        // branching to the rebuild turn would lose the transcript context
        assert_eq!(find_entry(&rebuilt, 1, &entries), None);
        assert_eq!(find_entry(&rebuilt, 0, &entries), None);

        let plain = vec![fwd("1", Some("transcript…")), fwd("2", Some("again")), fwd("3", Some("again"))];
        assert_eq!(find_entry(&plain, 0, &entries), Some("e0".into()));
        assert_eq!(find_entry(&plain, 1, &entries), Some("e1".into()));
        assert_eq!(find_entry(&plain, 2, &entries), Some("e2".into()));
        assert_eq!(find_entry(&plain, 2, &entries[..2]), None);
        assert_eq!(find_entry(&plain, 3, &entries), None);
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
