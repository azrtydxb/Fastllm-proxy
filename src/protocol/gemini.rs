//! Google Gemini `generateContent` / `streamGenerateContent`.
//!
//! Two differences drive everything here: the model is named in the **URL**
//! rather than the body, so the request translation decides the path as well
//! as the payload; and generation settings live under `generationConfig`
//! rather than at the top level. The assistant role is spelled `model`.

use serde::Deserialize;
use serde_json::json;

use super::{
    synthetic_id, AssistantMessage, Choice, Completion, OpenAiRequest, ResponseContext, SseEvent,
    StopField, StreamTranslator, TranslateError, TranslatedRequest, Usage,
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
    let (system, turns) = req.split_system()?;

    let contents: Vec<serde_json::Value> = turns
        .into_iter()
        .map(|(role, text)| {
            let role = if role == "assistant" { "model" } else { "user" };
            json!({"role": role, "parts": [{"text": text}]})
        })
        .collect();

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
struct Part {
    #[serde(default)]
    text: Option<String>,
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
                content: resp.text(),
            },
            finish_reason: finish_reason(reason).or(Some("stop".into())),
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
    if let Some(reason) = resp
        .candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
    {
        t.finish_reason = finish_reason(Some(reason));
    }
}
