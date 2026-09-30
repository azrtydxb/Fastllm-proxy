//! The Anthropic Messages API as a *frontend*: `POST /v1/messages`.
//!
//! [`anthropic`](super::anthropic) is the other direction -- an OpenAI-shaped
//! request translated for an Anthropic *backend*. This module faces the
//! client. An Anthropic-native caller (Claude Code, the Anthropic SDKs) is
//! translated to the OpenAI chat shape the rest of the proxy speaks, sent down
//! the ordinary request path, and the answer is translated back. Routing,
//! cache affinity, budgets, rate limits and RBAC therefore apply unchanged,
//! because the request they see is an ordinary chat completion.
//!
//! Everything is `serde_json::Value` in, `Value` out rather than typed
//! structs. The OpenAI side is whatever a vLLM, an OpenRouter or an Ollama
//! chooses to send, with vendor extras on every one, and a typed model of it
//! would reject or drop the fields nobody planned for.
//!
//! What has no OpenAI equivalent is refused by name rather than dropped, for
//! the reason [`super`] gives: a translator that silently does less than it was
//! asked is worse than none. Thinking blocks are the one deliberate omission --
//! see [`convert_assistant`]. (Reasoning in a *response* is translated, to
//! `thinking` blocks; it is only the ones a client sends back that are dropped.)

use bytes::Bytes;
use hyper::body::{Body, Frame};
use serde_json::{json, Map, Value};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::SseDecoder;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A translated request, plus what the response half needs to remember.
pub struct Translated {
    pub body: Vec<u8>,
    pub stream: bool,
    /// The client-facing model name, echoed back in the response: the client
    /// asked for this name, and a response naming the backend's internal one
    /// would look like a different model answered.
    pub model: String,
}

/// Anthropic Messages request in, OpenAI chat completions request out.
pub fn request_to_openai(body: &[u8]) -> Result<Translated, String> {
    let req: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = req
        .as_object()
        .ok_or("the request body must be a JSON object")?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or("model is required")?
        .to_string();
    let max_tokens = obj
        .get("max_tokens")
        .and_then(Value::as_u64)
        .ok_or("max_tokens is required")?;
    let raw = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages is required")?;
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut messages = Vec::with_capacity(raw.len() + 1);
    if let Some(system) = obj.get("system") {
        let text = system_text(system)?;
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    for m in raw {
        convert_message(m, &mut messages)?;
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert("stream".into(), json!(stream));
    if stream {
        // Anthropic reports usage in `message_delta`; the OpenAI stream only
        // carries it when asked, and a stream without it has nothing to put
        // there.
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    for key in ["temperature", "top_p", "top_k"] {
        if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(stops) = obj.get("stop_sequences").filter(|v| !v.is_null()) {
        out.insert("stop".into(), stops.clone());
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("every tool needs a name")?;
                Ok(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                    }
                }))
            })
            .collect::<Result<_, &str>>()?;
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if let Some(choice) = obj.get("tool_choice").filter(|v| !v.is_null()) {
        let mapped = match choice.get("type").and_then(Value::as_str) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => {
                let name = choice
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("tool_choice of type tool needs a name")?;
                json!({ "type": "function", "function": { "name": name } })
            }
            _ => return Err("unsupported tool_choice".into()),
        };
        out.insert("tool_choice".into(), mapped);
    }

    Ok(Translated {
        body: serde_json::to_vec(&Value::Object(out)).map_err(|e| e.to_string())?,
        stream,
        model,
    })
}

/// `system` is a string or a list of text blocks.
fn system_text(system: &Value) -> Result<String, String> {
    match system {
        Value::String(s) => Ok(s.clone()),
        Value::Array(blocks) => Ok(blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n")),
        Value::Null => Ok(String::new()),
        _ => Err("system must be a string or a list of text blocks".into()),
    }
}

fn convert_message(m: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let role = m
        .get("role")
        .and_then(Value::as_str)
        .ok_or("every message needs a role")?;
    let content = m.get("content").ok_or("every message needs content")?;
    let blocks = match content {
        Value::String(s) => {
            out.push(json!({ "role": role, "content": s }));
            return Ok(());
        }
        Value::Array(b) => b,
        _ => return Err("message content must be a string or a list of blocks".into()),
    };
    match role {
        "user" => convert_user(blocks, out),
        "assistant" => convert_assistant(blocks, out),
        other => Err(format!("unsupported message role {other:?}")),
    }
}

fn convert_user(blocks: &[Value], out: &mut Vec<Value>) -> Result<(), String> {
    let mut parts = Vec::new();
    let mut tool_results = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(json!({
                "type": "text",
                "text": b.get("text").and_then(Value::as_str).unwrap_or_default(),
            })),
            Some("image") => {
                let src = b.get("source").ok_or("an image block needs a source")?;
                let url = match src.get("type").and_then(Value::as_str) {
                    Some("base64") => format!(
                        "data:{};base64,{}",
                        src.get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png"),
                        src.get("data").and_then(Value::as_str).unwrap_or_default()
                    ),
                    Some("url") => src
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or("a url image source needs a url")?
                        .to_string(),
                    _ => return Err("unsupported image source".into()),
                };
                parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
            Some("tool_result") => tool_results.push(json!({
                "role": "tool",
                "tool_call_id": b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
                "content": tool_result_text(b.get("content")),
            })),
            Some(other) => return Err(format!("unsupported content block type {other:?}")),
            None => return Err("a content block needs a type".into()),
        }
    }
    // Tool results first: OpenAI requires the `tool` messages to follow the
    // assistant message that made the calls immediately, and Anthropic puts
    // the results and any follow-up text in the one user turn.
    out.extend(tool_results);
    if !parts.is_empty() {
        let all_text = parts
            .iter()
            .all(|p| p.get("type").and_then(Value::as_str) == Some("text"));
        let content = if all_text {
            Value::String(
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        } else {
            Value::Array(parts)
        };
        out.push(json!({ "role": "user", "content": content }));
    }
    Ok(())
}

/// Thinking blocks are dropped: they carry a signature only Anthropic can
/// verify, and no OpenAI-shaped backend has a field to receive them. Losing
/// them costs a model its own earlier reasoning on a later turn, not
/// correctness -- the visible answer and tool calls are what carry the
/// conversation.
fn convert_assistant(blocks: &[Value], out: &mut Vec<Value>) -> Result<(), String> {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text.push(t);
                }
            }
            Some("tool_use") => calls.push(json!({
                "id": b.get("id").and_then(Value::as_str).unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": b.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": b.get("input").unwrap_or(&json!({})).to_string(),
                },
            })),
            Some("thinking") | Some("redacted_thinking") => {}
            Some(other) => return Err(format!("unsupported content block type {other:?}")),
            None => return Err("a content block needs a type".into()),
        }
    }
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text.join("\n\n"))
        },
    );
    if !calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(calls));
    }
    out.push(Value::Object(msg));
    Ok(())
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The reasoning text of an OpenAI message or stream delta, under whichever
/// name the backend uses (`reasoning_content` is DeepSeek's, `reasoning` is
/// newer vLLM's and OpenRouter's).
fn reasoning_of(v: &Value) -> Option<&str> {
    v.get("reasoning_content")
        .or_else(|| v.get("reasoning"))
        .and_then(Value::as_str)
}

/// OpenAI `finish_reason` to Anthropic `stop_reason`.
fn stop_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        // `stop`, `content_filter` and anything a vendor invents: the model
        // stopped, and `end_turn` is the reading an Anthropic client expects.
        _ => "end_turn",
    }
}

/// The Anthropic `usage` block from an OpenAI one.
///
/// `input_tokens` excludes cached tokens in Anthropic's accounting and
/// `prompt_tokens` includes them, so the cached count is subtracted and
/// reported separately -- otherwise a client summing the two would count the
/// cached prefix twice.
fn usage_block(usage: &Value) -> Value {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(prompt);
    let mut block = json!({
        "input_tokens": prompt - cached,
        "output_tokens": completion,
    });
    if cached > 0 {
        block["cache_read_input_tokens"] = json!(cached);
    }
    block
}

/// `msg_…` from whatever id the backend gave the completion.
fn message_id(id: Option<&str>) -> String {
    let raw = id.unwrap_or("0");
    format!("msg_{}", raw.strip_prefix("chatcmpl-").unwrap_or(raw))
}

/// A non-streaming OpenAI completion in, an Anthropic message out.
pub fn completion_to_message(body: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let doc: Value =
        serde_json::from_slice(body).map_err(|e| format!("upstream sent invalid JSON: {e}"))?;
    let choice = doc
        .pointer("/choices/0")
        .ok_or("upstream response has no choices")?;
    let message = choice.get("message").unwrap_or(&Value::Null);

    let mut content = Vec::new();
    // A reasoning model spends its tokens here before it writes any answer;
    // with `max_tokens` small enough, this is all the reply contains. Dropping
    // it returned an empty message for a request that had worked. vLLM has
    // used both spellings.
    if let Some(thought) = reasoning_of(message).filter(|t| !t.is_empty()) {
        content.push(json!({ "type": "thinking", "thinking": thought, "signature": "" }));
    }
    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            content.push(json!({
                "type": "tool_use",
                "id": call
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("toolu_{i}"), str::to_string),
                "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default(),
                "input": serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({})),
            }));
        }
    }

    let out = json!({
        "id": message_id(doc.get("id").and_then(Value::as_str)),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(
            choice.get("finish_reason").and_then(Value::as_str).unwrap_or("stop")
        ),
        "stop_sequence": null,
        "usage": usage_block(doc.get("usage").unwrap_or(&Value::Null)),
    });
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// The Anthropic error type for an HTTP status.
fn error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// `{"type":"error","error":{…}}`.
pub fn error_body(status: u16, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type": "error",
        "error": { "type": error_type(status), "message": message },
    }))
    .unwrap_or_default()
}

/// An OpenAI-shaped error body (`{"error":{"message":…}}`, or a bare string)
/// re-shaped, keeping its message.
pub fn error_from_openai(status: u16, body: &[u8]) -> Vec<u8> {
    let doc: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = doc
        .pointer("/error/message")
        .or_else(|| doc.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    let message = if message.is_empty() {
        format!("upstream returned {status}")
    } else {
        message
    };
    error_body(status, &message)
}

/// A rough input-token count for `/v1/messages/count_tokens`.
///
/// An estimate, and documented as one: counting exactly needs the model's
/// tokenizer, which the proxy does not have for an arbitrary backend, and
/// asking the backend would put a network call in a code path that promises
/// none. About four characters a token is the usual rule of thumb, images get
/// a flat allowance.
pub fn estimate_input_tokens(body: &[u8]) -> Result<u64, String> {
    fn chars(v: &Value) -> usize {
        match v {
            Value::String(s) => s.chars().count(),
            Value::Array(a) => a.iter().map(chars).sum(),
            Value::Object(o) => {
                if o.get("type").and_then(Value::as_str) == Some("image") {
                    return 4096;
                }
                o.iter().map(|(k, v)| k.len() + chars(v)).sum()
            }
            _ => 0,
        }
    }
    let req: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let total: usize = ["system", "messages", "tools"]
        .iter()
        .filter_map(|k| req.get(k))
        .map(chars)
        .sum();
    Ok((total as u64).div_ceil(4).max(1))
}

fn frame(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

enum Open {
    Thinking(u32),
    Text(u32),
    Tool { block: u32, upstream: u64 },
}

/// OpenAI chat-completion chunks in, the Anthropic event sequence out:
/// `message_start`, then blocks of `content_block_start` /
/// `content_block_delta` / `content_block_stop`, then `message_delta` and
/// `message_stop`.
///
/// The end is deferred to the terminal `[DONE]` (or the stream ending)
/// because OpenAI sends `finish_reason` in one chunk and the usage in a later
/// one, and Anthropic wants both in a single `message_delta`.
pub struct StreamConverter {
    decoder: SseDecoder,
    model: String,
    id: Option<String>,
    started: bool,
    finished: bool,
    open: Option<Open>,
    next_block: u32,
    stop: Option<&'static str>,
    usage: Value,
}

impl StreamConverter {
    pub fn new(model: String) -> Self {
        Self {
            decoder: SseDecoder::default(),
            model,
            id: None,
            started: false,
            finished: false,
            open: None,
            next_block: 0,
            stop: None,
            usage: Value::Null,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in self.decoder.push(bytes) {
            if self.finished {
                break;
            }
            self.on_data(ev.data.trim(), &mut out);
        }
        out
    }

    /// The upstream ended. Closes the message if `[DONE]` never came, so a
    /// client always receives a complete, well-formed sequence.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.finished {
            self.close(&mut out);
        }
        out
    }

    /// The upstream broke mid-stream. Anthropic's own streams report this as
    /// an `error` event, which is what its SDKs know how to raise.
    pub fn abort(&mut self, message: &str) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        out.extend(frame(
            "error",
            &json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
        ));
        out
    }

    fn on_data(&mut self, data: &str, out: &mut Vec<u8>) {
        if data == "[DONE]" {
            self.close(out);
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| err.as_str())
                .unwrap_or("upstream error");
            self.finished = true;
            out.extend(frame(
                "error",
                &json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
            ));
            return;
        }
        if self.id.is_none() {
            self.id = chunk.get("id").and_then(Value::as_str).map(str::to_string);
        }
        self.start(out);
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = usage.clone();
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(thought) = reasoning_of(delta).filter(|t| !t.is_empty()) {
                self.thinking(thought, out);
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                self.text(text, out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_call(call, out);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(stop_reason(reason));
        }
    }

    fn start(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        out.extend(frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id(self.id.as_deref()),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                },
            }),
        ));
        out.extend(frame("ping", &json!({ "type": "ping" })));
    }

    fn close_block(&mut self, out: &mut Vec<u8>) {
        let index = match self.open.take() {
            Some(Open::Thinking(i)) | Some(Open::Text(i)) => i,
            Some(Open::Tool { block, .. }) => block,
            None => return,
        };
        out.extend(frame(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": index }),
        ));
    }

    fn open_block(&mut self, block: Value, out: &mut Vec<u8>) -> u32 {
        let index = self.next_block;
        self.next_block += 1;
        out.extend(frame(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": index, "content_block": block }),
        ));
        index
    }

    fn thinking(&mut self, thought: &str, out: &mut Vec<u8>) {
        let index = match self.open {
            Some(Open::Thinking(i)) => i,
            _ => {
                self.close_block(out);
                let i = self.open_block(
                    json!({ "type": "thinking", "thinking": "", "signature": "" }),
                    out,
                );
                self.open = Some(Open::Thinking(i));
                i
            }
        };
        out.extend(frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "thinking_delta", "thinking": thought },
            }),
        ));
    }

    fn text(&mut self, text: &str, out: &mut Vec<u8>) {
        let index = match self.open {
            Some(Open::Text(i)) => i,
            _ => {
                self.close_block(out);
                let i = self.open_block(json!({ "type": "text", "text": "" }), out);
                self.open = Some(Open::Text(i));
                i
            }
        };
        out.extend(frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": text },
            }),
        ));
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<u8>) {
        let upstream = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = call.get("id").and_then(Value::as_str);
        let continuing = matches!(self.open, Some(Open::Tool { upstream: u, .. }) if u == upstream && id.is_none());
        if !continuing {
            self.close_block(out);
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = id.map_or_else(|| format!("toolu_{upstream}"), str::to_string);
            let block = self.open_block(
                json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                out,
            );
            self.open = Some(Open::Tool { block, upstream });
        }
        if let (Some(args), Some(Open::Tool { block, .. })) = (
            call.pointer("/function/arguments")
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty()),
            &self.open,
        ) {
            out.extend(frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": block,
                    "delta": { "type": "input_json_delta", "partial_json": args },
                }),
            ));
        }
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        self.finished = true;
        self.start(out);
        self.close_block(out);
        // `message_start` could only say zero; the real prompt size is known
        // now, and Anthropic's `message_delta` usage may carry it.
        let usage = usage_block(&self.usage);
        out.extend(frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.stop.unwrap_or("end_turn"), "stop_sequence": null },
                "usage": usage,
            }),
        ));
        out.extend(frame("message_stop", &json!({ "type": "message_stop" })));
    }
}

/// A response body run through a [`StreamConverter`].
///
/// Every field is `Unpin`, so this needs no pin projection.
pub struct MessagesStream {
    inner: crate::proxy::ResBody,
    conv: StreamConverter,
    done: bool,
}

impl MessagesStream {
    pub fn new(inner: crate::proxy::ResBody, model: String) -> Self {
        Self {
            inner,
            conv: StreamConverter::new(model),
            done: false,
        }
    }
}

impl Body for MessagesStream {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        // A loop because most upstream frames translate to nothing on their
        // own, and an empty data frame toward the client is a protocol error.
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let Some(data) = frame.data_ref() else {
                        continue;
                    };
                    let out = this.conv.push(data);
                    if out.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    let out = this.conv.abort(&e.to_string());
                    if out.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Ready(None) => {
                    this.done = true;
                    let out = this.conv.finish();
                    if out.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn system_messages_and_parameters_translate() {
        let t = request_to_openai(
            br#"{"model":"m","max_tokens":64,"system":[{"type":"text","text":"be brief"}],
                "stop_sequences":["END"],"temperature":0.2,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        let out = v(&t.body);
        assert_eq!(
            out["messages"][0],
            json!({"role":"system","content":"be brief"})
        );
        assert_eq!(out["messages"][1], json!({"role":"user","content":"hi"}));
        assert_eq!(out["stop"], json!(["END"]));
        assert_eq!(out["max_tokens"], 64);
        assert!(!t.stream);
    }

    #[test]
    fn tool_use_and_tool_result_round_trip_into_openai_roles() {
        let t = request_to_openai(
            br#"{"model":"m","max_tokens":8,
              "tools":[{"name":"get","description":"d","input_schema":{"type":"object"}}],
              "tool_choice":{"type":"any"},
              "messages":[
                {"role":"assistant","content":[
                  {"type":"thinking","thinking":"hm","signature":"s"},
                  {"type":"text","text":"checking"},
                  {"type":"tool_use","id":"toolu_1","name":"get","input":{"a":1}}]},
                {"role":"user","content":[
                  {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"42"}]},
                  {"type":"text","text":"and?"}]}]}"#,
        )
        .unwrap();
        let out = v(&t.body);
        assert_eq!(out["tool_choice"], "required");
        assert_eq!(out["tools"][0]["function"]["name"], "get");
        let m = out["messages"].as_array().unwrap();
        assert_eq!(m[0]["content"], "checking");
        assert_eq!(m[0]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(m[0]["tool_calls"][0]["function"]["arguments"], r#"{"a":1}"#);
        // The result comes before the follow-up text, straight after the call.
        assert_eq!(
            m[1],
            json!({"role":"tool","tool_call_id":"toolu_1","content":"42"})
        );
        assert_eq!(m[2], json!({"role":"user","content":"and?"}));
    }

    #[test]
    fn what_cannot_be_expressed_is_refused_by_name() {
        let e = request_to_openai(
            br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":[{"type":"document"}]}]}"#,
        )
        .err()
        .unwrap();
        assert!(e.contains("document"), "{e}");
        assert!(request_to_openai(br#"{"model":"m","messages":[]}"#).is_err());
    }

    #[test]
    fn a_completion_becomes_a_message_with_mapped_stop_and_usage() {
        let out = v(&completion_to_message(
            br#"{"id":"chatcmpl-abc","choices":[{"finish_reason":"tool_calls","message":{
                "content":"ok","tool_calls":[{"id":"call_1","function":{"name":"get","arguments":"{\"a\":1}"}}]}}],
                "usage":{"prompt_tokens":100,"completion_tokens":7,
                         "prompt_tokens_details":{"cached_tokens":40}}}"#,
            "claude-x",
        )
        .unwrap());
        assert_eq!(out["id"], "msg_abc");
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0], json!({"type":"text","text":"ok"}));
        assert_eq!(out["content"][1]["input"], json!({"a":1}));
        assert_eq!(out["usage"]["input_tokens"], 60);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 40);
        assert_eq!(out["usage"]["output_tokens"], 7);
    }

    /// A reasoning model that runs out of `max_tokens` while still thinking
    /// returns `content: null` and its text in `reasoning`. That used to
    /// become an empty message.
    #[test]
    fn reasoning_becomes_a_thinking_block() {
        let out = v(&completion_to_message(
            br#"{"id":"x","choices":[{"finish_reason":"length","message":{"content":null,"reasoning":"hm"}}],
                "usage":{"prompt_tokens":1,"completion_tokens":40}}"#,
            "m",
        )
        .unwrap());
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "hm");
        assert_eq!(out["stop_reason"], "max_tokens");
    }

    #[test]
    fn streamed_reasoning_opens_its_own_block_before_the_answer() {
        let mut c = StreamConverter::new("m".into());
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"a\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"b\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut out = c.push(sse.as_bytes());
        out.extend(c.finish());
        let ev = events(&out);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(ev[2].1["content_block"]["type"], "thinking");
        assert_eq!(ev[4].1["delta"]["thinking"], "b");
        assert_eq!(ev[6].1["content_block"]["type"], "text");
    }

    fn events(bytes: &[u8]) -> Vec<(String, Value)> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter(|b| !b.is_empty())
            .map(|b| {
                let (e, d) = b.split_once('\n').unwrap();
                (
                    e.strip_prefix("event: ").unwrap().to_string(),
                    v(d.strip_prefix("data: ").unwrap().as_bytes()),
                )
            })
            .collect()
    }

    /// The whole sequence Anthropic clients depend on, from chunks split at
    /// arbitrary byte boundaries.
    #[test]
    fn a_stream_yields_the_anthropic_event_sequence() {
        let mut c = StreamConverter::new("claude-x".into());
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"get\",\"arguments\":\"{\\\"a\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut out = Vec::new();
        for piece in sse.as_bytes().chunks(13) {
            out.extend(c.push(piece));
        }
        out.extend(c.finish());
        let names: Vec<String> = events(&out).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let ev = events(&out);
        assert_eq!(ev[6].1["content_block"]["type"], "tool_use");
        assert_eq!(ev[6].1["index"], 1);
        assert_eq!(ev[8].1["delta"]["partial_json"], "\":1}");
        assert_eq!(ev[10].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(ev[10].1["usage"]["output_tokens"], 5);
    }

    #[test]
    fn a_stream_that_just_ends_still_closes_cleanly() {
        let mut c = StreamConverter::new("m".into());
        let names: Vec<String> = events(&c.finish()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            ["message_start", "ping", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn errors_are_reshaped_and_keep_their_message() {
        let out = v(&error_from_openai(
            429,
            br#"{"error":{"message":"slow down","code":429}}"#,
        ));
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(out["error"]["message"], "slow down");
    }

    #[test]
    fn token_counts_are_estimated_from_the_text() {
        let n = estimate_input_tokens(
            br#"{"model":"m","messages":[{"role":"user","content":"abcdabcdabcdabcd"}]}"#,
        )
        .unwrap();
        // 16 characters of text plus the keys, over four.
        assert!((4..=8).contains(&n), "{n}");
    }
}
