//! Anthropic Messages API (`POST /v1/messages`).
//!
//! The shape differences that matter: the system prompt is a top-level field
//! rather than a message, `max_tokens` is mandatory, stop sequences are called
//! `stop_sequences`, and the streaming protocol is a typed event sequence
//! rather than a series of identical chunks.

use serde::Deserialize;
use serde_json::json;

use serde_json::Value;

use super::{
    synthetic_id, AssistantMessage, Choice, Completion, ContentItem, OpenAiFunctionCall,
    OpenAiRequest, OpenAiToolCall, ResponseContext, SseEvent, StopField, StreamTranslator,
    TranslateError, TranslatedRequest, Turn, Usage,
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
    let tools = req.tools.take();
    let tool_choice = req.tool_choice.take();
    let response_format = req.response_format.take();
    let (system, turns) = req.split_system()?;

    let mut body = json!({
        "model": upstream_model,
        "max_tokens": max_tokens,
        "messages": merge_consecutive(turns)?,
    });
    let obj = body.as_object_mut().expect("constructed as an object");
    if let Some(system) = system {
        // Marked cacheable, as a content block rather than a bare string.
        //
        // Anthropic's prompt caching is explicit — nothing is cached unless a
        // block carries `cache_control` — and a cache hit costs 90% less than
        // the same input tokens. An OpenAI-format client has no way to ask for
        // this, so a translated backend was paying full price on every request
        // for a prefix that is identical across all of them.
        //
        // The system prompt is the right and only safe place to put the
        // breakpoint automatically: it is the one part of a chat request that
        // is stable across turns by construction. Marking a *message* would be
        // guessing at which prefix repeats.
        //
        // Below Anthropic's minimum cacheable length the marker is ignored
        // rather than an error, so there is nothing to check for here.
        obj.insert(
            "system".into(),
            json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"},
            }]),
        );
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
    // Structured output. Anthropic ignores OpenAI's `response_format`
    // outright, so a request carrying one used to be refused here; the native
    // spelling is `output_config.format`.
    //
    // Only a `json_schema` maps. A bare `{"type":"json_object"}` asks for
    // "some JSON" with no schema, which Anthropic has no equivalent for —
    // inventing an empty schema would constrain the model to `{}`, so it is
    // dropped and the model is merely asked in the prompt, which is what the
    // caller had before this existed.
    if let Some(schema) = response_format
        .as_ref()
        .and_then(super::response_format_schema)
    {
        obj.insert(
            "output_config".into(),
            json!({"format": {"type": "json_schema", "schema": schema}}),
        );
    }
    // `tool_choice: "none"` is expressed by offering no tools at all. That is
    // exact rather than approximate — a model with no tools cannot call one —
    // and it avoids depending on the `{"type":"none"}` form, which post-dates
    // the API version this adapter pins.
    let suppressed = tool_choice.as_ref().and_then(|c| c.as_str()) == Some("none");
    if let Some(tools) = tools.filter(|t| !t.is_empty() && !suppressed) {
        let declared: Vec<Value> = tools
            .iter()
            .filter_map(|t| t.function.as_ref())
            .map(|f| {
                let mut decl = json!({
                    "name": f.name,
                    // Required, and a tool that takes no arguments still has a
                    // schema: the empty object. Omitting it is a 400.
                    "input_schema": f.parameters.clone()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                });
                if let Some(d) = &f.description {
                    decl["description"] = json!(d);
                }
                decl
            })
            .collect();
        if !declared.is_empty() {
            obj.insert("tools".into(), json!(declared));
            if let Some(choice) = tool_choice.and_then(map_tool_choice) {
                obj.insert("tool_choice".into(), choice);
            }
        }
    }

    Ok(TranslatedRequest {
        body: serde_json::to_vec(&body).map_err(|e| TranslateError::Malformed(e.to_string()))?,
        subpath: "/messages".into(),
    })
}

/// OpenAI's `tool_choice` in Anthropic's spelling.
///
/// `"required"` becomes `any` — both mean "call something, your pick". A
/// choice this code does not recognise is dropped rather than guessed at:
/// `auto` is Anthropic's default and also the honest answer to "we do not know
/// what you asked for", where inventing a forced call would change what the
/// model does.
fn map_tool_choice(choice: Value) -> Option<Value> {
    match &choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            _ => None,
        },
        Value::Object(_) => {
            let name = choice.get("function")?.get("name")?.as_str()?;
            Some(json!({"type": "tool", "name": name}))
        }
        _ => None,
    }
}

/// One turn as Anthropic content blocks.
///
/// Returns a bare string for the ordinary text-only turn — the shape both this
/// API and its own clients use — and an array only once there is tool traffic
/// that a string cannot express.
fn blocks(turn: &Turn) -> Result<Vec<Value>, TranslateError> {
    let mut out = Vec::new();
    // Results first. Anthropic requires every `tool_result` to precede any
    // other block in the message that carries it.
    for r in &turn.tool_results {
        out.push(json!({
            "type": "tool_result",
            "tool_use_id": r.id,
            "content": r.text,
        }));
    }
    for item in &turn.content {
        match item {
            ContentItem::Text(t) if t.is_empty() => {}
            ContentItem::Text(t) => out.push(json!({"type": "text", "text": t})),
            ContentItem::Inline { mime, data } => {
                // Anthropic takes images inline and has no audio input at all.
                // Sending audio as an image block would be rejected upstream
                // with a message about the media type, several layers from the
                // client that sent it.
                if !mime.starts_with("image/") {
                    return Err(TranslateError::Unsupported(
                        "audio input for an Anthropic backend",
                    ));
                }
                out.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime, "data": data},
                }));
            }
            // Anthropic fetches the URL itself, which is why this is expressible
            // here and not for Gemini.
            ContentItem::Remote { url } => out.push(json!({
                "type": "image",
                "source": {"type": "url", "url": url},
            })),
        }
    }
    for call in &turn.tool_calls {
        out.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.function.name,
            // Anthropic wants the arguments as a real object where OpenAI
            // carries them as a string. Arguments that do not parse are sent as
            // an empty object rather than dropping the call: the model asked
            // for a tool, and the reason it was skipped would otherwise be
            // invisible on both sides.
            "input": serde_json::from_str::<Value>(&call.function.arguments)
                .unwrap_or_else(|_| json!({})),
        }));
    }
    Ok(out)
}

/// Fold consecutive same-role turns into one.
///
/// Anthropic rejects two `user` messages in a row, while OpenAI accepts them
/// and real clients emit them (a system-turned-user preamble followed by the
/// actual question). Merging is the difference between those clients working
/// and getting a 400 they cannot act on.
fn merge_consecutive(turns: Vec<Turn>) -> Result<Vec<Value>, TranslateError> {
    let mut out: Vec<(String, Vec<Value>, bool)> = Vec::with_capacity(turns.len());
    for turn in &turns {
        let mut next = blocks(turn)?;
        match out.last_mut() {
            Some((prev_role, prev, prev_plain)) if *prev_role == turn.role => {
                *prev_plain = *prev_plain && turn.only_text();
                // Two adjacent text blocks were one message before the split
                // and read as one afterwards, so they join with the blank line
                // rather than arriving as two blocks.
                match (prev.last_mut(), next.first()) {
                    (Some(last), Some(first))
                        if last["type"] == "text" && first["type"] == "text" =>
                    {
                        let joined = format!(
                            "{}\n\n{}",
                            last["text"].as_str().unwrap_or_default(),
                            first["text"].as_str().unwrap_or_default()
                        );
                        last["text"] = json!(joined);
                        next.remove(0);
                    }
                    _ => {}
                }
                prev.append(&mut next);
            }
            _ => out.push((turn.role.clone(), next, turn.only_text())),
        }
    }
    Ok(out
        .into_iter()
        .map(|(role, mut blocks, plain)| {
            // Merging can leave a result behind a text block; the ordering rule
            // is on the message, so it is re-applied after the merge.
            blocks.sort_by_key(|b| u8::from(b["type"] != "tool_result"));
            match blocks.as_slice() {
                [only] if plain && only["type"] == "text" => {
                    json!({"role": role, "content": only["text"]})
                }
                _ => json!({"role": role, "content": blocks}),
            }
        })
        .collect())
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
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// The tool arguments, as a real JSON object.
    #[serde(default)]
    input: Option<Value>,
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
    let content: String = msg
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect();
    let tool_calls: Vec<OpenAiToolCall> = msg
        .content
        .iter()
        .filter(|b| b.kind == "tool_use")
        .map(|b| OpenAiToolCall {
            id: b.id.clone().unwrap_or_default(),
            kind: "function".into(),
            function: OpenAiFunctionCall {
                name: b.name.clone().unwrap_or_default(),
                // Back to a string, which is how OpenAI carries arguments.
                arguments: b
                    .input
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".into()),
            },
        })
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
                // Null, not empty string, when the model only called tools —
                // the distinction clients branch on.
                content: (!content.is_empty() || tool_calls.is_empty()).then_some(content),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
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
    #[serde(default)]
    content_block: Option<Block>,
}

#[derive(Debug, Deserialize)]
struct EventDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    /// A slice of the open tool call's argument JSON. Deliberately a `String`
    /// and not a `Value`: mid-call it is a *fragment* — `{"loc` — that no JSON
    /// parser accepts, and it is forwarded to the client the same way.
    #[serde(default)]
    partial_json: Option<String>,
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
        "content_block_start" => {
            // Only a tool block needs announcing. A text block's first delta
            // carries everything the client needs.
            if let Some(block) = parsed.content_block.filter(|b| b.kind == "tool_use") {
                t.open_tool_call(
                    block.id.unwrap_or_default(),
                    block.name.unwrap_or_default(),
                    out,
                );
            }
        }
        "content_block_delta" => {
            let delta = parsed.delta.as_ref();
            if let Some(text) = delta.and_then(|d| d.text.as_deref()) {
                t.emit_text(text, out);
            }
            if let Some(fragment) = delta.and_then(|d| d.partial_json.as_deref()) {
                t.emit_tool_arguments(fragment, out);
            }
        }
        "content_block_stop" => t.close_tool_call(),
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
        // `ping` and `error`: nothing to forward. An `error` event mid-stream
        // leaves the client with a short answer, which is the same outcome as
        // the upstream connection dropping and is handled the same way.
        _ => {}
    }
}
