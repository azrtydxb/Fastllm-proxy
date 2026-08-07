//! Google Gemini `generateContent` / `streamGenerateContent`.
//!
//! Two differences drive everything here: the model is named in the **URL**
//! rather than the body, so the request translation decides the path as well
//! as the payload; and generation settings live under `generationConfig`
//! rather than at the top level. The assistant role is spelled `model`.

use serde::Deserialize;
use serde_json::json;

use serde_json::Value;

use super::{
    synthetic_id, synthetic_tool_id, AssistantMessage, Choice, Completion, OpenAiFunctionCall,
    OpenAiRequest, OpenAiToolCall, ResponseContext, SseEvent, StopField, StreamTranslator,
    TranslateError, TranslatedRequest, Turn, Usage,
};

pub fn translate_request(
    mut req: OpenAiRequest,
    upstream_model: &str,
    default_max_tokens: Option<u32>,
) -> Result<TranslatedRequest, TranslateError> {
    // Unlike Anthropic, Gemini treats an output cap as optional, so a missing
    // one is not an error — it is simply left unset and the model's own
    // default applies.
    let max_tokens = req.effective_max_tokens(default_max_tokens);
    let streaming = req.is_streaming();
    let temperature = req.temperature;
    let top_p = req.top_p;
    let stop = req.stop.take().map(StopField::into_vec).unwrap_or_default();
    let tools = req.tools.take();
    let tool_choice = req.tool_choice.take();
    let (system, turns) = req.split_system()?;

    let contents: Vec<Value> = turns.iter().map(parts).collect();

    let mut body = json!({ "contents": contents });
    let obj = body.as_object_mut().expect("constructed as an object");
    if let Some(system) = system {
        obj.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }

    let mut config = serde_json::Map::new();
    if let Some(n) = max_tokens {
        config.insert("maxOutputTokens".into(), json!(n));
    }
    if let Some(t) = temperature {
        config.insert("temperature".into(), json!(t));
    }
    if let Some(p) = top_p {
        config.insert("topP".into(), json!(p));
    }
    if !stop.is_empty() {
        config.insert("stopSequences".into(), json!(stop));
    }
    if !config.is_empty() {
        obj.insert("generationConfig".into(), config.into());
    }

    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        let declarations: Vec<Value> = tools
            .iter()
            .filter_map(|t| t.function.as_ref())
            .map(|f| {
                let mut decl = json!({"name": f.name});
                if let Some(d) = &f.description {
                    decl["description"] = json!(d);
                }
                // A no-argument tool omits `parameters` entirely here, where
                // Anthropic wants an empty schema. Sending `{}` to Gemini is a
                // 400.
                if let Some(schema) = &f.parameters {
                    let schema = prune_schema(schema.clone());
                    if schema
                        .get("properties")
                        .is_some_and(|p| p.as_object().is_some_and(|p| !p.is_empty()))
                    {
                        decl["parameters"] = schema;
                    }
                }
                decl
            })
            .collect();
        if !declarations.is_empty() {
            obj.insert(
                "tools".into(),
                json!([{"functionDeclarations": declarations}]),
            );
            if let Some(cfg) = tool_choice.and_then(map_tool_choice) {
                obj.insert("toolConfig".into(), json!({"functionCallingConfig": cfg}));
            }
        }
    }

    // `alt=sse` is what makes the streaming endpoint emit server-sent events
    // rather than a JSON array of responses; without it the "stream" is a
    // single document that only completes at the end, which would defeat the
    // point of streaming entirely.
    let subpath = if streaming {
        format!("/models/{upstream_model}:streamGenerateContent?alt=sse")
    } else {
        format!("/models/{upstream_model}:generateContent")
    };

    Ok(TranslatedRequest {
        body: serde_json::to_vec(&body).map_err(|e| TranslateError::Malformed(e.to_string()))?,
        subpath,
    })
}

/// One turn as a Gemini `Content`.
///
/// The assistant role is spelled `model`. Results go back under the `user`
/// role — Gemini has no separate role for them — which is why they were
/// attached to a user turn upstream.
fn parts(turn: &Turn) -> Value {
    let role = if turn.role == "assistant" {
        "model"
    } else {
        "user"
    };
    let mut parts: Vec<Value> = Vec::new();
    for r in &turn.tool_results {
        parts.push(json!({
            "functionResponse": {
                // Gemini pairs a result to its call by **name**; the id OpenAI
                // uses never appears on the wire here.
                "name": r.name,
                "response": response_object(&r.text),
            }
        }));
    }
    if !turn.text.is_empty() {
        parts.push(json!({"text": turn.text}));
    }
    for call in &turn.tool_calls {
        parts.push(json!({
            "functionCall": {
                "name": call.function.name,
                "args": serde_json::from_str::<Value>(&call.function.arguments)
                    .unwrap_or_else(|_| json!({})),
            }
        }));
    }
    json!({"role": role, "parts": parts})
}

/// Wrap a tool's output as the object Gemini requires.
///
/// A tool that returned a JSON object is passed through as itself. Anything
/// else — a bare number, a string, a list, unparseable text — is wrapped under
/// `output`, because `response` must be an object and a wrapper the model can
/// read beats a request the API rejects.
fn response_object(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => v,
        Ok(v) => json!({"output": v}),
        Err(_) => json!({"output": text}),
    }
}

/// Strip schema keywords Gemini rejects outright.
///
/// OpenAI tool schemas routinely carry `additionalProperties` and `$schema`
/// (every schema a JSON-Schema generator emits has them). Gemini's subset of
/// OpenAPI does not accept either and fails the whole request with a 400 that
/// names neither, so they are removed at every level rather than passed on.
fn prune_schema(schema: Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| !matches!(k.as_str(), "additionalProperties" | "$schema"))
                .map(|(k, v)| (k, prune_schema(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(prune_schema).collect()),
        other => other,
    }
}

/// OpenAI's `tool_choice` as a Gemini `functionCallingConfig`.
fn map_tool_choice(choice: Value) -> Option<Value> {
    match &choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"mode": "AUTO"})),
            "none" => Some(json!({"mode": "NONE"})),
            "required" => Some(json!({"mode": "ANY"})),
            _ => None,
        },
        Value::Object(_) => {
            let name = choice.get("function")?.get("name")?.as_str()?;
            // `ANY` plus a one-name allow-list is how Gemini spells "call this
            // specific tool"; there is no direct equivalent.
            Some(json!({"mode": "ANY", "allowedFunctionNames": [name]}))
        }
        _ => None,
    }
}

fn finish_reason(reason: Option<&str>) -> Option<String> {
    Some(
        match reason? {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            // Everything Google classifies as a content intervention maps to
            // the one OpenAI reason that says the model was stopped rather
            // than finished: SAFETY, RECITATION, BLOCKLIST, PROHIBITED_CONTENT,
            // SPII.
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
                "content_filter"
            }
            _ => "stop",
        }
        .to_string(),
    )
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
    #[serde(default)]
    response_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCall>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct FunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Option<Value>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
}

impl GenerateContentResponse {
    fn parts(&self) -> &[Part] {
        self.candidates
            .first()
            .and_then(|c| c.content.as_ref())
            .map(|c| c.parts.as_slice())
            .unwrap_or_default()
    }

    fn calls(&self) -> impl Iterator<Item = &FunctionCall> {
        self.parts().iter().filter_map(|p| p.function_call.as_ref())
    }

    fn text(&self) -> String {
        self.candidates
            .first()
            .and_then(|c| c.content.as_ref())
            .map(|c| {
                c.parts
                    .iter()
                    .filter_map(|p| p.text.as_deref())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }
}

pub fn translate_response(
    body: &[u8],
    ctx: &ResponseContext,
) -> Result<Completion, TranslateError> {
    let resp: GenerateContentResponse =
        serde_json::from_slice(body).map_err(|e| TranslateError::Malformed(e.to_string()))?;
    let usage = resp.usage_metadata.unwrap_or_default();
    let reason = resp
        .candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref());
    let tool_calls: Vec<OpenAiToolCall> = resp
        .calls()
        .enumerate()
        .map(|(i, c)| OpenAiToolCall {
            id: synthetic_tool_id(ctx.request_id, i as u32),
            kind: "function".into(),
            function: OpenAiFunctionCall {
                name: c.name.clone(),
                arguments: c
                    .args
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".into()),
            },
        })
        .collect();
    let text = resp.text();
    let has_calls = !tool_calls.is_empty();
    Ok(Completion {
        id: resp
            .response_id
            .clone()
            .unwrap_or_else(|| synthetic_id(ctx.request_id)),
        object: "chat.completion",
        created: super::now_secs(),
        model: ctx.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: (!text.is_empty() || tool_calls.is_empty()).then_some(text),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            },
            // Gemini reports `STOP` even when it stopped to call a tool, so the
            // presence of a call is the authoritative signal. Reporting "stop"
            // would tell the client the turn was finished and the calls it just
            // received needed no reply.
            finish_reason: if has_calls {
                Some("tool_calls".into())
            } else {
                finish_reason(reason).or(Some("stop".into()))
            },
        }],
        usage: Usage::new(usage.prompt_token_count, usage.candidates_token_count),
    })
}

pub fn on_event(t: &mut StreamTranslator, event: &SseEvent, out: &mut Vec<u8>) {
    let Ok(resp) = serde_json::from_str::<GenerateContentResponse>(&event.data) else {
        return;
    };
    if t.id.is_none() {
        if let Some(id) = resp.response_id.clone() {
            t.id = Some(id);
        }
    }
    // Gemini reports running totals on every event, so the last one seen is
    // authoritative — accumulating would multiply the count by the number of
    // chunks.
    if let Some(usage) = &resp.usage_metadata {
        t.prompt_tokens = usage.prompt_token_count;
        t.completion_tokens = usage.candidates_token_count;
    }
    t.emit_text(&resp.text(), out);
    // Gemini delivers a call complete in one event rather than fragmenting its
    // arguments, so there is nothing to accumulate — the whole call goes out as
    // one frame that a client can act on immediately.
    for call in resp.calls() {
        let id = synthetic_tool_id(t.ctx.request_id, t.next_tool_index);
        let args = call
            .args
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".into());
        t.emit_whole_tool_call(id, call.name.clone(), &args, out);
    }
    if let Some(reason) = resp
        .candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
    {
        // As in the non-streaming path: Gemini says `STOP` on the same event
        // that carries a call, so a call seen anywhere in this stream outranks
        // it. Without this the client is told the turn ended and never runs the
        // tool.
        t.finish_reason = if t.saw_tool_call {
            Some("tool_calls".into())
        } else {
            finish_reason(Some(reason))
        };
    }
}
