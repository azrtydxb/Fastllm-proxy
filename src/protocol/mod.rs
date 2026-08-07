//! Wire-protocol translation for upstreams that do not speak OpenAI.
//!
//! **This module is unreachable on the default path.** A backend is
//! [`Protocol::OpenAi`] unless an operator sets otherwise, and on that path the
//! request body is forwarded as-is and the response is never parsed at all —
//! which is the entire reason the proxy's measured overhead against a real
//! vLLM is zero. Translation is a second execution mode, entered only for a
//! backend explicitly configured for it, and `tests/passthrough_is_byte_exact.rs`
//! pins the boundary so it cannot erode.
//!
//! What lives here is deliberately narrow: enough of the OpenAI chat
//! completions surface to be genuinely useful against Anthropic and Gemini,
//! and an explicit refusal for everything else. A translator that silently
//! drops a field the caller sent is worse than no translator, because the
//! caller cannot tell it happened — so [`OpenAiRequest::check_supported`]
//! rejects with a named feature rather than quietly forwarding less than was
//! asked for.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub mod anthropic;
pub mod body;
pub mod gemini;

#[cfg(test)]
mod tests;

/// Which wire format an upstream speaks.
///
/// Stored per backend rather than per model: the same model name can be served
/// by an OpenAI-compatible gateway and by its vendor's native API at the same
/// time (Claude via OpenRouter and via `api.anthropic.com`), and those are two
/// backends of one pool that differ in nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
}

impl Protocol {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }

    /// Whether requests to this backend need translating at all. The one
    /// branch that keeps the OpenAI path free of every cost in this module.
    #[inline]
    pub fn is_passthrough(self) -> bool {
        matches!(self, Self::OpenAi)
    }
}

/// Why a request could not be translated.
///
/// `Unsupported` carries the feature name so the client is told what it asked
/// for that this build cannot express, instead of receiving a response that
/// silently did less.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    Unsupported(&'static str),
    Malformed(String),
    /// Anthropic requires `max_tokens`; the request omitted it and the backend
    /// has no configured default. See `Backend::default_max_tokens`.
    MissingMaxTokens,
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(feature) => write!(
                f,
                "{feature} is not supported when translating to a native provider protocol"
            ),
            Self::Malformed(why) => write!(f, "could not translate request: {why}"),
            Self::MissingMaxTokens => write!(
                f,
                "this provider requires max_tokens and the request did not set one; \
                 set max_tokens on the request, or default_max_tokens on the backend"
            ),
        }
    }
}

impl std::error::Error for TranslateError {}

/// A system prompt lifted out of the message list, and the conversation turns
/// that remain as `(role, text)`.
type SplitMessages = (Option<String>, Vec<(String, String)>);

/// A translated request: the body to send, and the path to send it to.
///
/// The path is part of the result because Gemini addresses the model in the
/// URL (`/models/{model}:generateContent`) rather than in the body, so the
/// two cannot be decided independently.
#[derive(Debug)]
pub struct TranslatedRequest {
    pub body: Vec<u8>,
    pub subpath: String,
}

// ---------------------------------------------------------------------------
// The OpenAI request, as much of it as we translate
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OpenAiRequest {
    #[serde(default)]
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// The current spelling; `max_tokens` is the deprecated one. Accepting
    /// both matters because clients are split across the rename and a request
    /// that set only the new name would otherwise look like it set neither,
    /// and fail against Anthropic for no reason the caller could see.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stop: Option<StopField>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,

    // Refused rather than translated. Each is `Value` because we only need to
    // know whether the caller sent it.
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub functions: Option<Value>,
    #[serde(default)]
    pub logprobs: Option<Value>,
    #[serde(default)]
    pub seed: Option<Value>,
    #[serde(default)]
    pub response_format: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StopField {
    One(String),
    Many(Vec<String>),
}

impl StopField {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Content>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

impl Content {
    /// Flatten to plain text, refusing anything that is not text.
    ///
    /// Images and audio parts are the interesting case: concatenating only the
    /// text parts of a multimodal message would send a coherent-looking
    /// request that had quietly discarded the image the whole question was
    /// about.
    fn into_text(self) -> Result<String, TranslateError> {
        match self {
            Self::Text(s) => Ok(s),
            Self::Parts(parts) => {
                let mut out = String::new();
                for part in parts {
                    if part.kind != "text" {
                        return Err(TranslateError::Unsupported("non-text content parts"));
                    }
                    out.push_str(part.text.as_deref().unwrap_or_default());
                }
                Ok(out)
            }
        }
    }
}

impl OpenAiRequest {
    pub fn parse(body: &[u8]) -> Result<Self, TranslateError> {
        serde_json::from_slice(body).map_err(|e| TranslateError::Malformed(e.to_string()))
    }

    /// Reject what this build cannot express, naming the feature.
    ///
    /// Ordered most-likely-first so the message a caller gets names the thing
    /// they are most likely to be doing, when a request uses several at once.
    pub fn check_supported(&self) -> Result<(), TranslateError> {
        if self.tools.is_some() || self.functions.is_some() || self.tool_choice.is_some() {
            return Err(TranslateError::Unsupported("tool and function calling"));
        }
        if self.response_format.is_some() {
            return Err(TranslateError::Unsupported("response_format"));
        }
        if self.n.is_some_and(|n| n > 1) {
            return Err(TranslateError::Unsupported("n greater than 1"));
        }
        if self.logprobs.is_some() {
            return Err(TranslateError::Unsupported("logprobs"));
        }
        if self.seed.is_some() {
            return Err(TranslateError::Unsupported("seed"));
        }
        Ok(())
    }

    fn is_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    fn wants_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .and_then(|o| o.include_usage)
            .unwrap_or(false)
    }

    /// `max_completion_tokens` wins when both are set — it is the current
    /// spelling, so a client sending both is most plausibly a library that
    /// fills the legacy field for compatibility.
    fn effective_max_tokens(&self, default: Option<u32>) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens).or(default)
    }

    /// Split system messages from the conversation.
    ///
    /// Both target protocols carry the system prompt outside the message list
    /// (`system` for Anthropic, `systemInstruction` for Gemini), so this is
    /// shared. Multiple system messages join with a blank line, which is what
    /// both vendors' own clients do.
    fn split_system(self) -> Result<SplitMessages, TranslateError> {
        let mut system: Vec<String> = Vec::new();
        let mut turns: Vec<(String, String)> = Vec::new();
        for msg in self.messages {
            let text = match msg.content {
                Some(c) => c.into_text()?,
                None => String::new(),
            };
            match msg.role.as_str() {
                "system" | "developer" => system.push(text),
                "user" | "assistant" => turns.push((msg.role, text)),
                "tool" | "function" => {
                    return Err(TranslateError::Unsupported("tool and function calling"))
                }
                other => {
                    return Err(TranslateError::Malformed(format!(
                        "unknown message role {other:?}"
                    )))
                }
            }
        }
        let system = if system.is_empty() {
            None
        } else {
            Some(system.join("\n\n"))
        };
        Ok((system, turns))
    }
}

// ---------------------------------------------------------------------------
// The OpenAI response, as we emit it
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Usage {
    fn new(prompt: u32, completion: u32) -> Self {
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Completion {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct Chunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Wall-clock seconds for the `created` field.
///
/// Not I/O — a `clock_gettime` on every platform this runs on — so it does not
/// breach the no-I/O-on-the-hot-path rule, and it is only reached in
/// translation mode anyway.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Response id for providers that do not supply one.
///
/// Derived from the routing prefix hash the request already computed, rather
/// than from a random source: the data plane has no RNG dependency in a
/// `--no-default-features` build, and a deterministic id makes the translation
/// tests assert on exact bytes instead of on a shape.
fn synthetic_id(request_id: u64) -> String {
    format!("chatcmpl-{request_id:016x}")
}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

/// One decoded SSE event.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE decoder.
///
/// Exists because upstream events arrive split across TCP reads at arbitrary
/// byte boundaries — including mid-event and mid-UTF-8-sequence. Buffering raw
/// bytes and only decoding at a `\n\n` boundary is what makes the UTF-8 step
/// safe: an event separator is always a character boundary, so a partial
/// multi-byte sequence can never be handed to `from_utf8`.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        // `\r\n\r\n` is legal in SSE and appears in the wild behind proxies
        // that rewrite line endings, so both separators are honoured.
        while let Some((end, sep_len)) = find_separator(&self.buf) {
            let raw = self.buf.drain(..end + sep_len).collect::<Vec<u8>>();
            let raw = &raw[..end];
            if let Ok(text) = std::str::from_utf8(raw) {
                if let Some(ev) = parse_event(text) {
                    events.push(ev);
                }
            }
        }
        events
    }
}

fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_event(text: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Comment lines (`:`) and unknown fields are ignored, per the SSE
        // spec — Anthropic's keep-alive `ping` arrives as one.
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent { event, data })
}

/// Serialise one chunk as an SSE frame.
fn sse_frame(chunk: &Chunk) -> Vec<u8> {
    let mut out = b"data: ".to_vec();
    // Serialising our own type: the only way this fails is a non-finite float,
    // and no field here is a float.
    out.extend_from_slice(&serde_json::to_vec(chunk).unwrap_or_default());
    out.extend_from_slice(b"\n\n");
    out
}

/// The terminator every OpenAI streaming client waits for.
pub const SSE_DONE: &[u8] = b"data: [DONE]\n\n";

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Translate a client request for a native backend.
///
/// `request_id` seeds [`synthetic_id`]; `default_max_tokens` comes from the
/// backend and is only consulted when the request set neither spelling of
/// `max_tokens`.
pub fn translate_request(
    protocol: Protocol,
    body: &[u8],
    upstream_model: &str,
    default_max_tokens: Option<u32>,
) -> Result<TranslatedRequest, TranslateError> {
    let req = OpenAiRequest::parse(body)?;
    req.check_supported()?;
    match protocol {
        Protocol::OpenAi => Err(TranslateError::Malformed(
            "passthrough backends are never translated".into(),
        )),
        Protocol::Anthropic => {
            anthropic::translate_request(req, upstream_model, default_max_tokens)
        }
        Protocol::Gemini => gemini::translate_request(req, upstream_model, default_max_tokens),
    }
}

/// What the response translator needs to know about the request it answers.
#[derive(Clone)]
pub struct ResponseContext {
    pub model: String,
    pub request_id: u64,
    pub streaming: bool,
    pub include_usage: bool,
}

impl ResponseContext {
    pub fn from_request(body: &[u8], model: String, request_id: u64) -> Self {
        let parsed = OpenAiRequest::parse(body).ok();
        Self {
            model,
            request_id,
            streaming: parsed.as_ref().is_some_and(|r| r.is_streaming()),
            include_usage: parsed.as_ref().is_some_and(|r| r.wants_usage()),
        }
    }
}

/// Streaming response translator.
///
/// Fed upstream bytes as they arrive, emits OpenAI-shaped SSE bytes. Holds the
/// usage it has seen so far, which is why a translated response never needs
/// the tail buffer: the numbers `TailBuffer` exists to recover have already
/// been parsed here, exactly rather than from a bounded guess at the tail.
pub struct StreamTranslator {
    decoder: SseDecoder,
    state: StreamState,
    ctx: ResponseContext,
    created: u64,
    id: Option<String>,
    prompt_tokens: u32,
    completion_tokens: u32,
    finish_reason: Option<String>,
    role_sent: bool,
    done: bool,
}

enum StreamState {
    Anthropic,
    Gemini,
}

impl StreamTranslator {
    pub fn new(protocol: Protocol, ctx: ResponseContext) -> Option<Self> {
        let state = match protocol {
            Protocol::OpenAi => return None,
            Protocol::Anthropic => StreamState::Anthropic,
            Protocol::Gemini => StreamState::Gemini,
        };
        Some(Self {
            decoder: SseDecoder::default(),
            state,
            created: now_secs(),
            id: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            finish_reason: None,
            role_sent: false,
            done: false,
            ctx,
        })
    }

    pub fn usage(&self) -> Usage {
        Usage::new(self.prompt_tokens, self.completion_tokens)
    }

    fn id(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| synthetic_id(self.ctx.request_id))
    }

    fn chunk(&self, delta: Delta, finish_reason: Option<String>) -> Chunk {
        Chunk {
            id: self.id(),
            object: "chat.completion.chunk",
            created: self.created,
            model: self.ctx.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage: None,
        }
    }

    /// Feed upstream bytes, get client bytes.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for event in self.decoder.push(bytes) {
            match self.state {
                StreamState::Anthropic => anthropic::on_event(self, &event, &mut out),
                StreamState::Gemini => gemini::on_event(self, &event, &mut out),
            }
        }
        out
    }

    /// Emit the trailing frames once the upstream body ends.
    ///
    /// Idempotent: a provider that closes the stream after its own terminal
    /// event and one that simply stops both end up here, and neither may emit
    /// `[DONE]` twice.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        if self.finish_reason.is_none() {
            self.finish_reason = Some("stop".into());
        }
        let mut final_chunk = self.chunk(Delta::default(), self.finish_reason.clone());
        if self.ctx.include_usage {
            final_chunk.usage = Some(self.usage());
        }
        out.extend_from_slice(&sse_frame(&final_chunk));
        out.extend_from_slice(SSE_DONE);
        out
    }

    fn emit_role(&mut self, out: &mut Vec<u8>) {
        if self.role_sent {
            return;
        }
        self.role_sent = true;
        let chunk = self.chunk(
            Delta {
                role: Some("assistant"),
                content: Some(String::new()),
            },
            None,
        );
        out.extend_from_slice(&sse_frame(&chunk));
    }

    fn emit_text(&mut self, text: &str, out: &mut Vec<u8>) {
        if text.is_empty() {
            return;
        }
        self.emit_role(out);
        let chunk = self.chunk(
            Delta {
                role: None,
                content: Some(text.to_string()),
            },
            None,
        );
        out.extend_from_slice(&sse_frame(&chunk));
    }
}

/// Translate a complete non-streaming response body.
pub fn translate_response(
    protocol: Protocol,
    body: &[u8],
    ctx: &ResponseContext,
) -> Result<(Vec<u8>, Usage), TranslateError> {
    let completion = match protocol {
        Protocol::OpenAi => {
            return Err(TranslateError::Malformed(
                "passthrough backends are never translated".into(),
            ))
        }
        Protocol::Anthropic => anthropic::translate_response(body, ctx)?,
        Protocol::Gemini => gemini::translate_response(body, ctx)?,
    };
    let usage = completion.usage;
    let bytes =
        serde_json::to_vec(&completion).map_err(|e| TranslateError::Malformed(e.to_string()))?;
    Ok((bytes, usage))
}
