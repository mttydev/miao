//! What a "what would the operator send next?" prediction is shown, and how
//! its answer is reduced to something the composer can accept.
//!
//! The call itself belongs to the Session's own route and credential, which
//! only the engine holds, so it stays in `runtime`. This module owns the two
//! pure steps around it.

use crate::protocol::Message;
use serde_json::Value;

/// The instruction the model answers. A prediction is one line of prompt, so
/// the model is told to reply with nothing else; a model that adds a preamble
/// is handled by [`one_line`] rather than trusted.
pub(crate) const SYSTEM: &str = "You watch a coding agent session and predict \
what the operator will type next. Reply with that prompt only: one line of \
plain text, at most 160 characters, no quotes, no numbering, no explanation. \
If the next step cannot be predicted, reply with an empty line.";

/// How much of the transcript the model sees: enough for the thread to be
/// legible, short enough that a suggestion costs a fraction of a turn.
const MESSAGES: usize = 6;
const PER_MESSAGE_CHARS: usize = 400;

/// The transcript tail, plus the draft the operator has typed so far.
pub(crate) fn context(history: &[Message], draft: Option<&str>) -> String {
    let mut lines = history
        .iter()
        .rev()
        .filter_map(|message| {
            let body = message_text(&message.content);
            let body = body.trim();
            if body.is_empty() {
                return None;
            }
            let head: String = body.chars().take(PER_MESSAGE_CHARS).collect();
            Some(format!("{}: {head}", message.role))
        })
        .take(MESSAGES)
        .collect::<Vec<_>>();
    lines.reverse();
    if let Some(draft) = draft.map(str::trim).filter(|draft| !draft.is_empty()) {
        lines.push(format!("draft: {draft}"));
    }
    lines.join("\n")
}

/// The one line worth offering: the first line that says something, with the
/// quoting and fencing a model wraps a one-line answer in removed, capped so a
/// rambling answer cannot fill the composer.
pub(crate) fn one_line(content: &Value) -> Option<String> {
    let text = message_text(content);
    let line = text
        .lines()
        .map(str::trim)
        .map(|line| {
            line.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '「' | '」' | '“' | '”'))
        })
        .map(|line| line.trim_start_matches("- ").trim())
        .find(|line| line.chars().any(char::is_alphanumeric))?;
    Some(line.chars().take(160).collect())
}

/// The text of one message body: either a plain string or the typed parts the
/// engine stores for a projected message.
fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: json!([{ "type": "text", "text": text }]),
            checkpoint: None,
            recorded_at_ms: None,
        }
    }

    #[test]
    fn the_draft_is_what_was_typed_and_the_tail_keeps_its_order() {
        let history = vec![message("user", "first"), message("assistant", "second")];
        let context = context(&history, Some("finish the "));
        assert_eq!(context, "user: first\nassistant: second\ndraft: finish the");
    }

    #[test]
    fn an_empty_history_without_a_draft_asks_nothing() {
        assert!(context(&[], None).is_empty());
        assert!(context(&[], Some("   ")).is_empty());
    }

    #[test]
    fn only_the_newest_messages_are_shown_and_each_is_capped() {
        let history: Vec<Message> = (0..10)
            .map(|index| message("user", &format!("{index}{}", "x".repeat(600))))
            .collect();
        let context = context(&history, None);
        assert!(
            !context.contains("user: 0x"),
            "the oldest message is dropped"
        );
        assert!(context.contains("user: 9x"));
        let longest = context.lines().map(|line| line.chars().count()).max();
        assert!(longest.is_some_and(|length| length <= PER_MESSAGE_CHARS + 6));
    }

    #[test]
    fn a_fenced_answer_is_reduced_to_its_sentence() {
        let content = json!([{ "type": "text", "text": "```\nadd the missing test\n```" }]);
        assert_eq!(one_line(&content).as_deref(), Some("add the missing test"));
    }

    #[test]
    fn quoting_and_bullets_do_not_survive_and_a_rambling_answer_is_capped() {
        let content = json!([{ "type": "text", "text": "\"- 修复 cache 的 key\"\nand then more" }]);
        assert_eq!(one_line(&content).as_deref(), Some("修复 cache 的 key"));
        let long = "a".repeat(400);
        let content = json!([{ "type": "text", "text": long }]);
        assert_eq!(
            one_line(&content).map(|line| line.chars().count()),
            Some(160)
        );
    }

    #[test]
    fn a_reply_that_says_nothing_is_no_suggestion() {
        let content = json!([{ "type": "text", "text": "```\n\n```" }]);
        assert_eq!(one_line(&content), None);
    }
}
