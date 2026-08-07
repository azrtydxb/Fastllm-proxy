//! Anthropic Messages API (`POST /v1/messages`).
//!
//! The shape differences that matter: the system prompt is a top-level field
//! rather than a message, `max_tokens` is mandatory, stop sequences are called
//! `stop_sequences`, and the streaming protocol is a typed event sequence
//! rather than a series of identical chunks.

use serde::Deserialize;
use serde_json::json;

use super::{
    synthetic_id, AssistantMessage, Choice, Completion, OpenAiRequest, ResponseContext, SseEvent,
    StopField, StreamTranslator, TranslateError, TranslatedRequest, Usage,
};

/// The version header Anthropic requires on every request. Sent by the
/// protocol adapter rather than configured per backend, because it is a
/// property of the wire format this code implements, not of the deployment —
/// an operator who had to supply it could get it wrong, and a mismatched
/// version changes response shapes underneath the translator.
pub const API_VERSION: &str = "2023-06-01";

pub fn translate_request(
    mut req: OpenAiRequest,
    upstream_model: &str,
    default_max_tokens: Option<u32>,
) -> Result<TranslatedRequest, TranslateError> {
    let max_tokens = req
        .effective_max_tokens(default_max_tokens)
        .ok_or(TranslateError::MissingMaxTokens)?;
    let streaming = req.is_streaming();
    let temperature = req.temperature;
    let top_p = req.top_p;
    let stop = req.stop.take().map(StopField::into_vec).unwrap_or_default();
    let (system, turns) = req.split_system()?;

    let mut body = json!({
        "model": upstream_model,
        "max_tokens": max_tokens,
        "messages": merge_consecutive(turns),
    });
    let obj = body.as_object_mut().expect("constructed as an object");
    if let Some(system) = system {
        obj.insert("system".into(), json!(system));
    }
    if let Some(t) = temperature {
        obj.insert("temperature".into(), json!(t));
    }
    if let Some(p) = top_p {
        obj.insert("top_p".into(), json!(p));
    }
    if !stop.is_empty() {
        obj.insert("stop_sequences".into(), json!(stop));
    }
    if streaming {
        obj.insert("stream".into(), json!(true));
    }

    Ok(TranslatedRequest {
        body: serde_json::to_vec(&body).map_err(|e| TranslateError::Malformed(e.to_string()))?,
        subpath: "/messages".into(),
    })
}

/// Fold consecutive same-role turns into one.
///
/// Anthropic rejects two `user` messages in a row, while OpenAI accepts them
/// and real clients emit them (a system-turned-user preamble followed by the
/// actual question). Merging is the difference between those clients working
/// and getting a 400 they cannot act on.
fn merge_consecutive(turns: Vec<(String, String)>) -> Vec<serde_json::Value> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(turns.len());
    for (role, text) in turns {
        match out.last_mut() {
            Some((prev_role, prev_text)) if *prev_role == role => {
                prev_text.push_str("\n\n");
                prev_text.push_str(&text);
            }
            _ => out.push((role, text)),
        }
    }
    out.into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

fn finish_reason(stop_reason: Option<&str>) -> Option<String> {
    Some(
        match stop_reason? {
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            // `end_turn`, `stop_sequence`, and anything a future API version
            // adds: a natural end is the only honest default, and inventing a
            // reason we do not understand would be worse than the generic one.
            _ => "stop",
        }
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Non-streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<MessageUsage>,
}

#[derive(Debug, Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct MessageUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

pub fn translate_response(
    body: &[u8],
    ctx: &ResponseContext,
) -> Result<Completion, TranslateError> {
    let msg: Message =
        serde_json::from_slice(body).map_err(|e| TranslateError::Malformed(e.to_string()))?;
    // Only text blocks contribute. A `tool_use` block cannot appear, because
    // a request carrying tools was refused before it was ever sent.
    let content: String = msg
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect();
    let usage = msg.usage.unwrap_or_default();
    Ok(Completion {
        id: msg.id.unwrap_or_else(|| synthetic_id(ctx.request_id)),
        object: "chat.completion",
        created: super::now_secs(),
        model: ctx.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content,
            },
            finish_reason: finish_reason(msg.stop_reason.as_deref()).or(Some("stop".into())),
        }],
        usage: Usage::new(usage.input_tokens, usage.output_tokens),
    })
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    delta: Option<EventDelta>,
    #[serde(default)]
    usage: Option<MessageUsage>,
}

#[derive(Debug, Deserialize)]
struct EventDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

pub fn on_event(t: &mut StreamTranslator, event: &SseEvent, out: &mut Vec<u8>) {
    let Ok(parsed) = serde_json::from_str::<StreamEvent>(&event.data) else {
        // An event we cannot parse is dropped rather than fatal: the stream is
        // already committed to the client by this point, and killing it over
        // one unrecognised frame would turn a cosmetic upstream change into a
        // truncated answer.
        return;
    };
    // The `type` field inside the payload is authoritative; the SSE `event:`
    // line duplicates it and is only a fallback for a provider that omits it.
    let kind = if parsed.kind.is_empty() {
        event.event.as_deref().unwrap_or_default()
    } else {
        parsed.kind.as_str()
    };

    match kind {
        "message_start" => {
            if let Some(msg) = parsed.message {
                if let Some(id) = msg.id {
                    t.id = Some(id);
                }
                if let Some(usage) = msg.usage {
                    t.prompt_tokens = usage.input_tokens;
                    t.completion_tokens = usage.output_tokens;
                }
            }
            t.emit_role(out);
        }
        "content_block_delta" => {
            if let Some(text) = parsed.delta.as_ref().and_then(|d| d.text.as_deref()) {
                t.emit_text(text, out);
            }
        }
        "message_delta" => {
            if let Some(reason) = parsed.delta.as_ref().and_then(|d| d.stop_reason.as_deref()) {
                t.finish_reason = finish_reason(Some(reason));
            }
            // Replaced, not accumulated: Anthropic reports the running total
            // for the message, and `message_start` already seeded a value.
            if let Some(usage) = parsed.usage {
                t.completion_tokens = usage.output_tokens;
            }
        }
        "message_stop" => {
            let tail = t.finish();
            out.extend_from_slice(&tail);
        }
        // `ping`, `content_block_start`, `content_block_stop`, `error`: nothing
        // to forward. An `error` event mid-stream leaves the client with a
        // short answer, which is the same outcome as the upstream connection
        // dropping and is handled the same way.
        _ => {}
    }
}
