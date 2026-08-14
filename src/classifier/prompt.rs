//! What text a request is classified on.
//!
//! # The bug this exists to fix
//!
//! The classifier used to be handed the **raw request body** — JSON syntax,
//! role labels, system prompt and all — and read only the first 128 tokens of
//! it. Meanwhile the control plane builds a class centroid from example prompts
//! an operator typed: bare text. Two different distributions, and
//! nearest-centroid classification has no way to notice.
//!
//! Measured over 4,750 held-out prompts (`docs/classifier/measurements.md`),
//! centroids from bare text throughout:
//!
//! | query shape | coding precision | coding recall |
//! |---|---|---|
//! | bare prompt (what every documented number assumed) | 71.7% | 91.3% |
//! | minimal JSON body | 72.3% | 92.0% |
//! | body with a system prompt | 97.8% | **30.0%** |
//! | turn 4 of a conversation | **0.0%** | **0.0%** |
//!
//! The JSON wrapping itself turned out to be harmless. What is not harmless is
//! everything the window fills up with before reaching the user's words: a
//! system prompt costs two thirds of recall, and by the fourth turn the class
//! is undetectable — the prompt being classified sits at the end of the body,
//! where a 128-token window never reaches.
//!
//! Overall accuracy stayed at 96.8% throughout, because the class is a small
//! share of traffic. That is the base-rate trap `docs/classifier.md` warns
//! about, hiding a total failure.
//!
//! And the mean margin *rose* as accuracy collapsed (0.198 bare, 0.225 at turn
//! four), so a `min_margin` floor is no defence: the classifier is confidently
//! wrong, and no threshold an operator can set will filter that.
//!
//! # What is classified instead
//!
//! The last user message, on its own. That is the turn being answered, it is
//! what the operator's examples look like, and it is what every accuracy number
//! in the docs was measured on.

use serde::Deserialize;

/// Enough of a chat request to find the current turn. Everything else in the
/// body is skipped by `serde_json` without being materialised.
#[derive(Deserialize)]
struct Peek<'a> {
    #[serde(default, borrow)]
    messages: Vec<Message<'a>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(default)]
    role: &'a str,
    #[serde(default, borrow)]
    content: Option<Content<'a>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content<'a> {
    #[serde(borrow)]
    Text(&'a str),
    /// Multimodal parts. Only the text ones can be classified; an image
    /// contributes nothing an embedding of words can use.
    Parts(Vec<Part<'a>>),
    /// An owned fallback for a string carrying JSON escapes, which cannot
    /// borrow from the input.
    Owned(String),
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(default, borrow)]
    text: Option<&'a str>,
}

impl Content<'_> {
    fn text(&self) -> String {
        match self {
            Self::Text(s) => (*s).to_string(),
            Self::Owned(s) => s.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// The text to classify: the last user message in the request.
///
/// `None` when the body is not a chat request, has no user message, or that
/// message has no text — a multimodal turn carrying only an image, say. The
/// caller treats that as "unclassified" rather than guessing, which is the
/// same thing it already does when no class clears its floor.
///
/// Deliberately **not** the whole conversation. Earlier turns are context for
/// the model, not a description of what is being asked now, and including them
/// is exactly what made a fourth-turn coding question undetectable.
pub fn text_to_classify(body: &[u8]) -> Option<String> {
    let peek: Peek = serde_json::from_slice(body).ok()?;
    let text = peek
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")?
        .content
        .as_ref()?
        .text();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_user_message_is_what_gets_classified() {
        // The whole point: turn four of a conversation classifies on turn four.
        let body = br#"{"model":"auto","messages":[
            {"role":"system","content":"You are a helpful assistant."},
            {"role":"user","content":"Help me plan a dinner party."},
            {"role":"assistant","content":"Three courses works well."},
            {"role":"user","content":"Why does this Rust code fail the borrow checker?"}
        ]}"#;
        assert_eq!(
            text_to_classify(body).as_deref(),
            Some("Why does this Rust code fail the borrow checker?")
        );
    }

    /// A system prompt is instructions to the model, not a description of what
    /// the user is asking. Including it cost two thirds of recall.
    #[test]
    fn the_system_prompt_is_not_part_of_the_question() {
        let body = br#"{"messages":[
            {"role":"system","content":"You are a careful assistant. Cite sources."},
            {"role":"user","content":"What is the capital of France?"}
        ]}"#;
        let got = text_to_classify(body).unwrap();
        assert_eq!(got, "What is the capital of France?");
        assert!(!got.contains("careful assistant"));
    }

    /// No JSON syntax, no role labels, no keys — the text an operator's example
    /// prompts look like, which is what the centroids were built from.
    #[test]
    fn no_json_scaffolding_survives_into_the_classified_text() {
        let body =
            br#"{"model":"auto","max_tokens":512,"messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(text_to_classify(body).as_deref(), Some("hello"));
    }

    #[test]
    fn a_multimodal_turn_contributes_its_text_and_ignores_the_image() {
        let body = br#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"What is wrong with this diagram?"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]}"#;
        assert_eq!(
            text_to_classify(body).as_deref(),
            Some("What is wrong with this diagram?")
        );
    }

    #[test]
    fn a_turn_with_no_text_at_all_is_left_unclassified() {
        // Guessing from an image would be worse than falling through to the
        // next routing rule, which is what `None` does.
        let body = br#"{"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]}"#;
        assert_eq!(text_to_classify(body), None);
    }

    #[test]
    fn escaped_content_still_reads_correctly() {
        // A borrowed &str cannot represent this, so the untagged fallback has
        // to catch it — otherwise a prompt with a quote in it silently fails
        // to classify.
        let body = br#"{"messages":[{"role":"user","content":"why does \"cargo build\" fail?"}]}"#;
        assert_eq!(
            text_to_classify(body).as_deref(),
            Some(r#"why does "cargo build" fail?"#)
        );
    }

    #[test]
    fn a_body_that_is_not_a_chat_request_classifies_nothing() {
        assert_eq!(text_to_classify(b"not json"), None);
        assert_eq!(text_to_classify(br#"{"input":"embeddings request"}"#), None);
        assert_eq!(text_to_classify(br#"{"messages":[]}"#), None);
        // Assistant-only history has no question in it to classify.
        assert_eq!(
            text_to_classify(br#"{"messages":[{"role":"assistant","content":"hi"}]}"#),
            None
        );
        // Whitespace is not a prompt.
        assert_eq!(
            text_to_classify(br#"{"messages":[{"role":"user","content":"   "}]}"#),
            None
        );
    }
}
