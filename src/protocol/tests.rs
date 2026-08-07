//! Translation is asserted on exact bytes, both directions.
//!
//! A test that only checks "the translator produced some JSON" would pass on a
//! translator that silently dropped the system prompt or halved the token
//! count — which is precisely the failure mode this module has to be trusted
//! not to have. So the request tests compare against the literal payload the
//! provider should receive, and the streaming tests reassemble the emitted SSE
//! and compare the reconstructed text and usage.

use super::*;
use serde_json::{json, Value};

fn ctx() -> ResponseContext {
    ResponseContext {
        model: "claude-sonnet-4".into(),
        request_id: 0xdeadbeef,
        streaming: true,
        include_usage: true,
    }
}

fn translated(protocol: Protocol, body: Value, max_default: Option<u32>) -> (Value, String) {
    let out = translate_request(
        protocol,
        body.to_string().as_bytes(),
        "upstream-model",
        max_default,
    )
    .expect("translation should succeed");
    (
        serde_json::from_slice(&out.body).expect("translated body is JSON"),
        out.subpath,
    )
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

#[test]
fn anthropic_request_lifts_the_system_prompt_out_of_the_message_list() {
    let (body, subpath) = translated(
        Protocol::Anthropic,
        json!({
            "model": "whatever",
            "max_tokens": 100,
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Hello"},
            ],
        }),
        None,
    );
    assert_eq!(subpath, "/messages");
    assert_eq!(
        body,
        json!({
            "model": "upstream-model",
            "max_tokens": 100,
            "system": "Be terse.",
            "messages": [{"role": "user", "content": "Hello"}],
        })
    );
}

#[test]
fn anthropic_request_maps_generation_settings_and_stop_sequences() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 7,
            "temperature": 0.5,
            "top_p": 0.9,
            "stop": "END",
            "stream": true,
        }),
        None,
    );
    assert_eq!(body["temperature"], json!(0.5));
    assert_eq!(body["top_p"], json!(0.9));
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert_eq!(body["stream"], json!(true));
}

/// Anthropic rejects two consecutive `user` turns; OpenAI clients emit them.
#[test]
fn anthropic_request_merges_consecutive_same_role_turns() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "user", "content": "second"},
                {"role": "assistant", "content": "reply"},
            ],
        }),
        None,
    );
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "first\n\nsecond"},
            {"role": "assistant", "content": "reply"},
        ])
    );
}

/// The whole point of `default_max_tokens`: the request omitted it, and
/// Anthropic will not accept the request without one.
#[test]
fn anthropic_request_without_max_tokens_uses_the_backend_default() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"messages": [{"role": "user", "content": "Hi"}]}),
        Some(512),
    );
    assert_eq!(body["max_tokens"], json!(512));
}

#[test]
fn anthropic_request_without_max_tokens_or_default_is_refused_by_name() {
    let err = translate_request(
        Protocol::Anthropic,
        json!({"messages": [{"role": "user", "content": "Hi"}]})
            .to_string()
            .as_bytes(),
        "m",
        None,
    )
    .unwrap_err();
    assert_eq!(err, TranslateError::MissingMaxTokens);
    assert!(err.to_string().contains("default_max_tokens"));
}

#[test]
fn max_completion_tokens_is_honoured_and_wins_over_the_legacy_spelling() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1,
            "max_completion_tokens": 99,
        }),
        None,
    );
    assert_eq!(body["max_tokens"], json!(99));
}

#[test]
fn gemini_request_puts_the_model_in_the_url_and_settings_under_generation_config() {
    let (body, subpath) = translated(
        Protocol::Gemini,
        json!({
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"},
            ],
            "max_tokens": 64,
            "temperature": 0.2,
            "stop": ["A", "B"],
        }),
        None,
    );
    assert_eq!(subpath, "/models/upstream-model:generateContent");
    assert_eq!(
        body,
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hello"}]},
                {"role": "model", "parts": [{"text": "Hi"}]},
            ],
            "systemInstruction": {"parts": [{"text": "Be terse."}]},
            "generationConfig": {
                "maxOutputTokens": 64,
                "temperature": 0.2,
                "stopSequences": ["A", "B"],
            },
        })
    );
}

#[test]
fn gemini_streaming_request_asks_for_server_sent_events() {
    let (_, subpath) = translated(
        Protocol::Gemini,
        json!({"messages": [{"role": "user", "content": "Hi"}], "stream": true}),
        None,
    );
    assert_eq!(
        subpath,
        "/models/upstream-model:streamGenerateContent?alt=sse"
    );
}

/// Gemini has no mandatory output cap, so omitting one must not be an error
/// the way it is for Anthropic.
#[test]
fn gemini_request_without_max_tokens_omits_the_field_rather_than_failing() {
    let (body, _) = translated(
        Protocol::Gemini,
        json!({"messages": [{"role": "user", "content": "Hi"}]}),
        None,
    );
    assert!(body.get("generationConfig").is_none());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

fn refusal(body: Value) -> TranslateError {
    translate_request(
        Protocol::Anthropic,
        body.to_string().as_bytes(),
        "m",
        Some(10),
    )
    .unwrap_err()
}

#[test]
fn unsupported_features_are_refused_by_name_rather_than_silently_dropped() {
    let base = json!({"messages": [{"role": "user", "content": "Hi"}]});
    let cases: &[(&str, Value, &str)] = &[
        ("tools", json!([{"type": "function"}]), "tool and function"),
        ("tool_choice", json!("auto"), "tool and function"),
        ("functions", json!([{"name": "f"}]), "tool and function"),
        (
            "response_format",
            json!({"type": "json_object"}),
            "response_format",
        ),
        ("logprobs", json!(true), "logprobs"),
        ("seed", json!(42), "seed"),
    ];
    for (field, value, expected) in cases {
        let mut body = base.clone();
        body[*field] = value.clone();
        let err = refusal(body);
        assert!(
            matches!(err, TranslateError::Unsupported(_)),
            "{field} should be refused, got {err:?}"
        );
        assert!(
            err.to_string().contains(expected),
            "{field}'s error should name the feature, got {err}"
        );
    }
}

#[test]
fn more_than_one_choice_is_refused_but_exactly_one_is_fine() {
    assert!(matches!(
        refusal(json!({"messages": [{"role": "user", "content": "Hi"}], "n": 2})),
        TranslateError::Unsupported("n greater than 1")
    ));
    assert!(translate_request(
        Protocol::Anthropic,
        json!({"messages": [{"role": "user", "content": "Hi"}], "n": 1})
            .to_string()
            .as_bytes(),
        "m",
        Some(10),
    )
    .is_ok());
}

/// The dangerous case: concatenating only the text parts would send a
/// coherent-looking request that had thrown away the image being asked about.
#[test]
fn multimodal_content_is_refused_rather_than_flattened_to_its_text_parts() {
    let err = refusal(json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is in this image?"},
            {"type": "image_url", "image_url": {"url": "http://example/x.png"}},
        ]}],
    }));
    assert_eq!(err, TranslateError::Unsupported("non-text content parts"));
}

#[test]
fn an_all_text_multipart_message_is_accepted_and_concatenated() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({
            "max_tokens": 5,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ]}],
        }),
        None,
    );
    assert_eq!(body["messages"][0]["content"], json!("ab"));
}

#[test]
fn a_passthrough_backend_is_never_translated() {
    assert!(translate_request(Protocol::OpenAi, b"{}", "m", None).is_err());
    assert!(StreamTranslator::new(Protocol::OpenAi, ctx()).is_none());
}

// ---------------------------------------------------------------------------
// SSE decoding
// ---------------------------------------------------------------------------

#[test]
fn sse_events_split_across_reads_are_reassembled() {
    let stream = "event: a\ndata: {\"x\":1}\n\nevent: b\ndata: {\"y\":2}\n\n";
    // One byte at a time is the pathological case: every event boundary, and
    // every multi-byte character, is split.
    let mut decoder = SseDecoder::default();
    let mut events = Vec::new();
    for byte in stream.as_bytes() {
        events.extend(decoder.push(&[*byte]));
    }
    assert_eq!(
        events,
        vec![
            SseEvent {
                event: Some("a".into()),
                data: "{\"x\":1}".into()
            },
            SseEvent {
                event: Some("b".into()),
                data: "{\"y\":2}".into()
            },
        ]
    );
}

#[test]
fn sse_decoding_survives_a_split_inside_a_multi_byte_character() {
    let stream = "data: {\"t\":\"héllo → wörld\"}\n\n".as_bytes();
    for split in 1..stream.len() {
        let mut decoder = SseDecoder::default();
        let mut events = decoder.push(&stream[..split]);
        events.extend(decoder.push(&stream[split..]));
        assert_eq!(events.len(), 1, "split at {split} lost the event");
        assert!(events[0].data.contains("héllo → wörld"));
    }
}

#[test]
fn sse_decoding_accepts_crlf_line_endings() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(b"event: ping\r\ndata: {}\r\n\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "{}");
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------

/// Reassemble what a client would see: the concatenated `delta.content`, the
/// finish reason, whether `[DONE]` arrived, and the usage on the final chunk.
fn replay(out: &[u8]) -> (String, Option<String>, bool, Option<Usage>) {
    let text = String::from_utf8(out.to_vec()).expect("emitted SSE is UTF-8");
    let mut content = String::new();
    let mut finish = None;
    let mut done = false;
    let mut usage = None;
    for frame in text.split("\n\n") {
        let Some(data) = frame.trim().strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            done = true;
            continue;
        }
        let chunk: Value = serde_json::from_str(data).expect("each frame is JSON");
        assert_eq!(chunk["object"], json!("chat.completion.chunk"));
        if let Some(c) = chunk["choices"][0]["delta"]["content"].as_str() {
            content.push_str(c);
        }
        if let Some(r) = chunk["choices"][0]["finish_reason"].as_str() {
            finish = Some(r.to_string());
        }
        if chunk["usage"].is_object() {
            usage = Some(Usage {
                prompt_tokens: chunk["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                completion_tokens: chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
                total_tokens: chunk["usage"]["total_tokens"].as_u64().unwrap_or(0) as u32,
            });
        }
    }
    (content, finish, done, usage)
}

const ANTHROPIC_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

#[test]
fn anthropic_stream_becomes_openai_chunks_with_exact_usage() {
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = t.push(ANTHROPIC_STREAM.as_bytes());
    out.extend(t.finish());

    let (content, finish, done, usage) = replay(&out);
    assert_eq!(content, "Hello world");
    assert_eq!(
        finish.as_deref(),
        Some("length"),
        "max_tokens maps to length"
    );
    assert!(done, "the client never sees the end without [DONE]");
    assert_eq!(usage, Some(Usage::new(11, 7)));
    assert_eq!(t.usage(), Usage::new(11, 7));
}

/// The reason a translated response needs no tail buffer: the numbers are
/// parsed exactly, not recovered from a bounded guess at the end of the body.
#[test]
fn anthropic_stream_reports_the_same_usage_byte_by_byte_as_in_one_read() {
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = Vec::new();
    for byte in ANTHROPIC_STREAM.as_bytes() {
        out.extend(t.push(&[*byte]));
    }
    out.extend(t.finish());
    let (content, _, done, usage) = replay(&out);
    assert_eq!(content, "Hello world");
    assert!(done);
    assert_eq!(usage, Some(Usage::new(11, 7)));
}

#[test]
fn anthropic_message_stop_ends_the_stream_and_finish_is_not_emitted_twice() {
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let out = t.push(ANTHROPIC_STREAM.as_bytes());
    let after = t.finish();
    assert!(
        after.is_empty(),
        "message_stop already terminated the stream"
    );
    assert_eq!(
        String::from_utf8_lossy(&out).matches("[DONE]").count(),
        1,
        "exactly one [DONE]"
    );
}

const GEMINI_STREAM: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}],\"role\":\"model\"}}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1},\"responseId\":\"resp_1\"}\n\n",
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}],\"role\":\"model\"}}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n",
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":3}}\n\n",
);

#[test]
fn gemini_stream_becomes_openai_chunks_and_takes_the_last_usage_not_the_sum() {
    let mut t = StreamTranslator::new(Protocol::Gemini, ctx()).unwrap();
    let mut out = t.push(GEMINI_STREAM.as_bytes());
    out.extend(t.finish());

    let (content, finish, done, usage) = replay(&out);
    assert_eq!(content, "Hello!");
    assert_eq!(finish.as_deref(), Some("stop"));
    assert!(done);
    // Running totals: summing the three events would report 6 completion
    // tokens for a 3-token answer.
    assert_eq!(usage, Some(Usage::new(5, 3)));
}

#[test]
fn gemini_content_filtering_is_reported_as_content_filter() {
    let mut t = StreamTranslator::new(Protocol::Gemini, ctx()).unwrap();
    let mut out = t.push(
        b"data: {\"candidates\":[{\"finishReason\":\"SAFETY\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":0}}\n\n",
    );
    out.extend(t.finish());
    let (_, finish, done, _) = replay(&out);
    assert_eq!(finish.as_deref(), Some("content_filter"));
    assert!(done);
}

/// A stream that simply stops — no terminal event — must still be closed out
/// properly, or the client hangs waiting for `[DONE]`.
#[test]
fn a_truncated_stream_is_still_terminated_for_the_client() {
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = t.push(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
    );
    out.extend(t.finish());
    let (_, finish, done, _) = replay(&out);
    assert!(done);
    assert_eq!(finish.as_deref(), Some("stop"));
}

#[test]
fn an_unparseable_event_is_dropped_rather_than_killing_a_committed_stream() {
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = t.push(b"event: garbage\ndata: not json at all\n\n");
    out.extend(t.push(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    ));
    out.extend(t.finish());
    let (content, _, done, _) = replay(&out);
    assert_eq!(content, "ok");
    assert!(done);
}

#[test]
fn usage_is_withheld_from_the_stream_unless_the_client_asked_for_it() {
    let mut without = StreamTranslator::new(
        Protocol::Anthropic,
        ResponseContext {
            include_usage: false,
            ..ctx()
        },
    )
    .unwrap();
    let mut out = without.push(ANTHROPIC_STREAM.as_bytes());
    out.extend(without.finish());
    let (_, _, _, usage) = replay(&out);
    assert_eq!(usage, None, "no stream_options.include_usage was set");
    // Still known internally — this is what budgets and rate limits consume.
    assert_eq!(without.usage(), Usage::new(11, 7));
}

// ---------------------------------------------------------------------------
// Non-streaming translation
// ---------------------------------------------------------------------------

#[test]
fn a_complete_anthropic_response_becomes_a_chat_completion() {
    let body = json!({
        "id": "msg_99",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello"}, {"type": "text", "text": " there"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 4},
    });
    let (out, usage) =
        translate_response(Protocol::Anthropic, body.to_string().as_bytes(), &ctx()).unwrap();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["id"], json!("msg_99"));
    assert_eq!(v["object"], json!("chat.completion"));
    assert_eq!(v["model"], json!("claude-sonnet-4"));
    assert_eq!(v["choices"][0]["message"]["role"], json!("assistant"));
    assert_eq!(v["choices"][0]["message"]["content"], json!("Hello there"));
    assert_eq!(v["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(usage, Usage::new(3, 4));
    assert_eq!(v["usage"]["total_tokens"], json!(7));
}

#[test]
fn a_complete_gemini_response_becomes_a_chat_completion() {
    let body = json!({
        "candidates": [{
            "content": {"parts": [{"text": "42"}], "role": "model"},
            "finishReason": "MAX_TOKENS",
        }],
        "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 2},
    });
    let (out, usage) =
        translate_response(Protocol::Gemini, body.to_string().as_bytes(), &ctx()).unwrap();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], json!("42"));
    assert_eq!(v["choices"][0]["finish_reason"], json!("length"));
    assert_eq!(usage, Usage::new(8, 2));
    // No `responseId` in the payload, so the id is derived from the request's
    // own routing hash rather than invented at random.
    assert_eq!(v["id"], json!("chatcmpl-00000000deadbeef"));
}
