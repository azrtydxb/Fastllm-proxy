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
use std::collections::HashMap;
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
/// that remain.
type SplitMessages = (Option<String>, Vec<Turn>);

/// One conversation turn, after roles are normalised but before either native
/// protocol's shape is applied.
///
/// Carries the tool traffic rather than flattening it to text, because both
/// native protocols nest calls and results *inside* messages while OpenAI keeps
/// them alongside — so the mapping cannot be done one message at a time.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    /// `"user"` or `"assistant"`.
    pub role: String,
    /// Text and media, in the order the client wrote them.
    pub content: Vec<ContentItem>,
    /// Calls this assistant turn made.
    pub tool_calls: Vec<OpenAiToolCall>,
    /// Results this turn carries. Attached to the *user* turn that follows the
    /// assistant's calls, which is where both native protocols expect them.
    pub tool_results: Vec<ToolResult>,
}

/// One tool result, carrying both keys: Anthropic pairs it back to the call by
/// `id`, Gemini by function `name`.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub text: String,
}

impl Turn {
    fn is_empty(&self) -> bool {
        self.content
            .iter()
            .all(|c| c.as_text().is_some_and(str::is_empty))
            && self.tool_calls.is_empty()
            && self.tool_results.is_empty()
    }

    /// Whether this turn is text and nothing else — the shape both adapters
    /// keep emitting as a bare string rather than a block array.
    fn only_text(&self) -> bool {
        self.content.iter().all(|c| c.as_text().is_some())
    }
}

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

    #[serde(default)]
    pub tools: Option<Vec<OpenAiTool>>,
    /// `"auto"` / `"none"` / `"required"`, or `{"type":"function","function":
    /// {"name":...}}`. Left as `Value` because each protocol re-shapes it
    /// differently and there is nothing shared to model.
    #[serde(default)]
    pub tool_choice: Option<Value>,

    // Refused rather than translated. Each is `Value` because we only need to
    // know whether the caller sent it.
    #[serde(default)]
    pub n: Option<u32>,
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
    /// Calls the assistant made on a previous turn. Present on
    /// `role: "assistant"` messages in a transcript that used tools.
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    /// Which call a `role: "tool"` message answers. The pairing is by id in
    /// OpenAI's shape and by *position inside a message* in both native
    /// protocols, which is the whole reason history needs a real mapper rather
    /// than a per-message translation.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Legacy `role: "function"` name. Accepted because clients still emit it.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: OpenAiFunctionCall,
}

fn default_tool_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiFunctionCall {
    pub name: String,
    /// JSON, as a **string**. OpenAI sends arguments serialised; both native
    /// protocols want a real object, so this is parsed on the way out and
    /// re-serialised on the way back.
    #[serde(default)]
    pub arguments: String,
}

/// A tool the caller offered, as OpenAI declares it.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiTool {
    #[serde(default)]
    pub function: Option<OpenAiFunctionDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiFunctionDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema. Passed through untouched — both native protocols take the
    /// same dialect, so translating it would only be an opportunity to lose
    /// something.
    #[serde(default)]
    pub parameters: Option<Value>,
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
    #[serde(default)]
    pub image_url: Option<ImageUrl>,
    #[serde(default)]
    pub input_audio: Option<InputAudio>,
}

#[derive(Debug, Deserialize)]
pub struct ImageUrl {
    /// Either a `data:` URL carrying the bytes inline, or a remote one.
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct InputAudio {
    /// Base64, always — OpenAI has no remote form for audio.
    pub data: String,
    /// `"wav"` or `"mp3"`; a bare extension, not a media type.
    #[serde(default)]
    pub format: Option<String>,
}

/// One piece of a message, after roles are normalised but before either native
/// protocol's shape is applied.
#[derive(Debug, Clone)]
pub enum ContentItem {
    Text(String),
    /// Media the client sent inline, still base64 exactly as it arrived.
    Inline {
        mime: String,
        data: String,
    },
    /// Media named by a URL the proxy does not resolve.
    ///
    /// Fetching it would be a network call while serving a request, which this
    /// proxy does not do — see `tests/no_io_on_hot_path.rs`. So the URL is
    /// handed to the provider to fetch, and refused for a provider that cannot.
    Remote {
        url: String,
    },
}

impl ContentItem {
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// Split a `data:` URL into its media type and base64 payload.
///
/// Only the base64 form is accepted. The percent-encoded form is legal in the
/// RFC and decoding it would mean re-encoding to base64 for both providers —
/// work on the request path for a shape no OpenAI client emits.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    let mime = if mime.is_empty() {
        "application/octet-stream"
    } else {
        mime
    };
    Some((mime.to_string(), data.to_string()))
}

impl Content {
    /// Flatten to plain text, refusing anything that is not text.
    ///
    /// For the places where only text is meaningful — a system prompt, a tool
    /// result — concatenating just the text parts of a multimodal message
    /// would send a coherent-looking request that had quietly discarded the
    /// image the whole question was about.
    fn into_text(self) -> Result<String, TranslateError> {
        let mut out = String::new();
        for item in self.into_items()? {
            match item {
                ContentItem::Text(t) => out.push_str(&t),
                _ => return Err(TranslateError::Unsupported("media in this position")),
            }
        }
        Ok(out)
    }

    /// Preserve the parts, and their order.
    ///
    /// Order carries meaning — "what is wrong with this?" before an image reads
    /// differently from the same words after it — so parts are kept in
    /// sequence rather than being sorted into text-then-media.
    fn into_items(self) -> Result<Vec<ContentItem>, TranslateError> {
        let parts = match self {
            Self::Text(s) => return Ok(vec![ContentItem::Text(s)]),
            Self::Parts(parts) => parts,
        };
        let mut out: Vec<ContentItem> = Vec::with_capacity(parts.len());
        for part in parts {
            match part.kind.as_str() {
                // Adjacent text joins rather than becoming two blocks: it was
                // one message before the client split it, and both providers
                // read a single string more predictably than a list of
                // fragments.
                "text" => {
                    let text = part.text.unwrap_or_default();
                    match out.last_mut() {
                        Some(ContentItem::Text(prev)) => prev.push_str(&text),
                        _ => out.push(ContentItem::Text(text)),
                    }
                }
                "image_url" => {
                    let url = part.image_url.map(|i| i.url).ok_or_else(|| {
                        TranslateError::Malformed("image_url part has no url".into())
                    })?;
                    out.push(match parse_data_url(&url) {
                        Some((mime, data)) => ContentItem::Inline { mime, data },
                        None => ContentItem::Remote { url },
                    });
                }
                "input_audio" => {
                    let audio = part.input_audio.ok_or_else(|| {
                        TranslateError::Malformed("input_audio part has no audio".into())
                    })?;
                    // OpenAI names the container (`wav`), providers want a
                    // media type (`audio/wav`).
                    let mime = format!("audio/{}", audio.format.as_deref().unwrap_or("wav"));
                    out.push(ContentItem::Inline {
                        mime,
                        data: audio.data,
                    });
                }
                other => {
                    return Err(TranslateError::Unsupported(match other {
                        "file" => "file content parts",
                        _ => "an unrecognised content part type",
                    }))
                }
            }
        }
        Ok(out)
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
        // `tools` and `tool_choice` are translated. `functions` is the
        // pre-2023 spelling that OpenAI itself deprecated; supporting it would
        // mean a second mapping for the same feature, and every client that
        // still emits it also accepts `tools`.
        if self.functions.is_some() {
            return Err(TranslateError::Unsupported(
                "the deprecated `functions` parameter — use `tools`",
            ));
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

    /// Split system messages out, and fold the tool traffic into turns.
    ///
    /// Both target protocols carry the system prompt outside the message list
    /// (`system` for Anthropic, `systemInstruction` for Gemini), so that part
    /// is shared. Multiple system messages join with a blank line, which is
    /// what both vendors' own clients do.
    ///
    /// The tool part is why this cannot be a per-message translation. OpenAI
    /// puts a call on the assistant message and its result in a *separate*
    /// `role: "tool"` message keyed by id; both native protocols nest the
    /// result inside the following user message. So results are attached to
    /// the turn that follows the call, and the id→name map built on the way
    /// past exists because Gemini keys results by function **name** and never
    /// sees the id at all.
    fn split_system(self) -> Result<SplitMessages, TranslateError> {
        let mut system: Vec<String> = Vec::new();
        let mut turns: Vec<Turn> = Vec::new();
        // Call id → function name, so a later `role: "tool"` message can be
        // given the name Gemini needs.
        let mut names: HashMap<String, String> = HashMap::new();

        for msg in self.messages {
            // A system prompt and a tool result are text by definition; only a
            // user or assistant turn can carry media, so the cheap flattening
            // stays where it is still correct.
            let is_turn = matches!(msg.role.as_str(), "user" | "assistant");
            let (text, content) = match (msg.content, is_turn) {
                (Some(c), true) => (String::new(), c.into_items()?),
                (Some(c), false) => (c.into_text()?, Vec::new()),
                (None, _) => (String::new(), Vec::new()),
            };
            match msg.role.as_str() {
                "system" | "developer" => system.push(text),
                "assistant" => {
                    let tool_calls = msg.tool_calls.unwrap_or_default();
                    for call in &tool_calls {
                        names.insert(call.id.clone(), call.function.name.clone());
                    }
                    turns.push(Turn {
                        role: "assistant".into(),
                        content,
                        tool_calls,
                        tool_results: Vec::new(),
                    });
                }
                "user" => turns.push(Turn {
                    role: "user".into(),
                    content,
                    ..Turn::default()
                }),
                "tool" | "function" => {
                    let id = msg.tool_call_id.clone().unwrap_or_default();
                    // `role: "function"` predates ids and identifies the call
                    // by name only; `name` is also the fallback for a client
                    // that omits `tool_call_id`.
                    let name = names
                        .get(&id)
                        .cloned()
                        .or_else(|| msg.name.clone())
                        .unwrap_or_else(|| id.clone());
                    let result = ToolResult { id, name, text };
                    // Consecutive results belong in one message: a model that
                    // made three calls in parallel gets one reply carrying all
                    // three, which is the shape both native protocols require.
                    match turns.last_mut() {
                        Some(t) if t.role == "user" && t.content.is_empty() => {
                            t.tool_results.push(result)
                        }
                        _ => turns.push(Turn {
                            role: "user".into(),
                            tool_results: vec![result],
                            ..Turn::default()
                        }),
                    }
                }
                other => {
                    return Err(TranslateError::Malformed(format!(
                        "unknown message role {other:?}"
                    )))
                }
            }
        }
        turns.retain(|t| !t.is_empty());
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
    /// `null` when the model answered with tool calls and no prose. Serialised
    /// rather than skipped, because that is what OpenAI itself emits and
    /// clients branch on the key being present.
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// A fragment of a tool call, as OpenAI streams one.
///
/// Everything but `index` is optional because the call is delivered in pieces:
/// the first frame carries `index`, `id`, `type` and the function name, and
/// every frame after it carries only another slice of `arguments`. The client
/// concatenates them.
#[derive(Debug, Serialize, Default)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    pub function: FunctionDelta,
}

#[derive(Debug, Serialize, Default)]
pub struct FunctionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A slice of the JSON argument text, **not** a JSON value. Forwarded
    /// without parsing: Anthropic streams partial JSON that is not valid on its
    /// own until the last fragment arrives, and parsing mid-flight would either
    /// fail or force buffering the whole call before emitting anything.
    pub arguments: String,
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

/// Tool-call id for a provider that does not supply one.
///
/// Gemini identifies a call by function name alone, but OpenAI clients pair a
/// result back to its call by id and send that id in the next request — so one
/// has to be invented, and it has to be stable for the length of the response.
/// Derived from the request id for the same reason as [`synthetic_id`]: no RNG
/// dependency, and translation tests can assert exact bytes.
fn synthetic_tool_id(request_id: u64, index: u32) -> String {
    format!("call_{request_id:016x}_{index}")
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
    /// Next OpenAI `tool_calls[].index` to hand out. Counts tool calls only,
    /// where Anthropic's block index counts text blocks too — so the two are
    /// not interchangeable and this cannot be taken from the wire.
    next_tool_index: u32,
    /// The call currently receiving argument fragments, if any. A single slot
    /// suffices because both protocols emit content blocks strictly in
    /// sequence, never interleaved.
    open_tool: Option<u32>,
    saw_tool_call: bool,
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
            next_tool_index: 0,
            open_tool: None,
            saw_tool_call: false,
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
            // A stream that produced tool calls ended because the model wants
            // them run. Saying "stop" would tell the client the turn is over
            // and the calls it just received need no reply.
            self.finish_reason = Some(if self.saw_tool_call {
                "tool_calls".into()
            } else {
                "stop".to_string()
            });
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
                ..Delta::default()
            },
            None,
        );
        out.extend_from_slice(&sse_frame(&chunk));
    }

    /// Begin a tool call: emit the frame carrying its id and name.
    ///
    /// Returns nothing — the index is remembered as the open call, and every
    /// argument fragment until [`Self::close_tool_call`] belongs to it.
    fn open_tool_call(&mut self, id: String, name: String, out: &mut Vec<u8>) {
        self.emit_role(out);
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.open_tool = Some(index);
        self.saw_tool_call = true;
        let chunk = self.chunk(
            Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index,
                    id: Some(id),
                    kind: Some("function"),
                    function: FunctionDelta {
                        name: Some(name),
                        arguments: String::new(),
                    },
                }]),
                ..Delta::default()
            },
            None,
        );
        out.extend_from_slice(&sse_frame(&chunk));
    }

    /// Forward a slice of the open call's argument JSON.
    fn emit_tool_arguments(&mut self, fragment: &str, out: &mut Vec<u8>) {
        let Some(index) = self.open_tool else {
            // Arguments with no open call: a provider frame we did not
            // understand. Dropping the fragment beats attributing it to the
            // wrong call, which would corrupt an argument list the client then
            // executes.
            return;
        };
        if fragment.is_empty() {
            return;
        }
        let chunk = self.chunk(
            Delta {
                tool_calls: Some(vec![ToolCallDelta {
                    index,
                    function: FunctionDelta {
                        arguments: fragment.to_string(),
                        ..FunctionDelta::default()
                    },
                    ..ToolCallDelta::default()
                }]),
                ..Delta::default()
            },
            None,
        );
        out.extend_from_slice(&sse_frame(&chunk));
    }

    fn close_tool_call(&mut self) {
        self.open_tool = None;
    }

    /// A whole tool call in one frame, for a provider that does not fragment
    /// its arguments.
    fn emit_whole_tool_call(&mut self, id: String, name: String, args: &str, out: &mut Vec<u8>) {
        self.open_tool_call(id, name, out);
        self.emit_tool_arguments(args, out);
        self.close_tool_call();
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
                ..Delta::default()
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
