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
            // A content block, not a bare string, so it can carry the cache
            // breakpoint. Anthropic accepts both spellings.
            "system": [{"type": "text", "text": "Be terse.",
                        "cache_control": {"type": "ephemeral"}}],
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
        ("functions", json!([{"name": "f"}]), "`functions`"),
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

/// A system prompt has no multimodal form in either protocol, and quietly
/// keeping only its text would throw away the image being asked about.
#[test]
fn media_outside_a_conversation_turn_is_refused_rather_than_dropped() {
    let err = refusal(json!({
        "messages": [
            {"role": "system", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
            ]},
            {"role": "user", "content": "Hi"},
        ],
    }));
    assert_eq!(err, TranslateError::Unsupported("media in this position"));
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

// ---------------------------------------------------------------------------
// Tool calling
// ---------------------------------------------------------------------------

/// A transcript in the shape a client sends back on the second turn: the tools
/// on offer, the call the model made, and the result of running it.
fn tool_transcript() -> Value {
    json!({
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function":
                    {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "{\"c\":17}"},
        ],
        "tools": [{"type": "function", "function": {
            "name": "get_weather",
            "description": "Current weather",
            "parameters": {"type": "object", "additionalProperties": false,
                "properties": {"city": {"type": "string"}}, "required": ["city"]},
        }}],
    })
}

/// The whole reason history needs a real mapper: OpenAI keeps the call on the
/// assistant message and its result in a separate one, where Anthropic nests
/// both inside messages and pairs them by id.
#[test]
fn anthropic_request_nests_the_call_and_its_result_inside_messages() {
    let (body, _) = translated(Protocol::Anthropic, tool_transcript(), None);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "get_weather",
                 "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "{\"c\":17}"}
            ]},
        ]),
        "arguments become a real object, and the result pairs back by id"
    );
    assert_eq!(
        body["tools"],
        json!([{
            "name": "get_weather",
            "description": "Current weather",
            "input_schema": {"type": "object", "additionalProperties": false,
                "properties": {"city": {"type": "string"}}, "required": ["city"]},
        }]),
        "the schema passes through untouched — Anthropic takes the same dialect"
    );
}

/// Gemini pairs a result to its call by function *name*; the id never reaches
/// the wire, so it has to be carried across from the assistant message.
#[test]
fn gemini_request_pairs_the_result_to_its_call_by_name() {
    let (body, _) = translated(Protocol::Gemini, tool_transcript(), None);
    assert_eq!(
        body["contents"],
        json!([
            {"role": "user", "parts": [{"text": "Weather in Paris?"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"c": 17}}}
            ]},
        ])
    );
    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations": [{
            "name": "get_weather",
            "description": "Current weather",
            // `additionalProperties` is gone: every JSON-Schema generator emits
            // it and Gemini rejects the whole request over it.
            "parameters": {"type": "object",
                "properties": {"city": {"type": "string"}}, "required": ["city"]},
        }]}])
    );
}

/// A tool that returned something other than an object still has to reach
/// Gemini as one, because `response` is required to be an object.
#[test]
fn gemini_wraps_a_non_object_tool_result() {
    let body = json!({
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "count", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c", "content": "42 widgets"},
        ],
    });
    let (body, _) = translated(Protocol::Gemini, body, None);
    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["response"],
        json!({"output": "42 widgets"})
    );
}

/// Parallel calls come back as several `role: "tool"` messages in a row, and
/// both native protocols want them in one reply rather than one message each.
#[test]
fn consecutive_tool_results_become_a_single_message() {
    let body = json!({
        "max_tokens": 10,
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "g", "arguments": "{}"}},
            ]},
            {"role": "tool", "tool_call_id": "a", "content": "1"},
            {"role": "tool", "tool_call_id": "b", "content": "2"},
        ],
    });
    let (body, _) = translated(Protocol::Anthropic, body, None);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "a", "content": "1"},
            {"type": "tool_result", "tool_use_id": "b", "content": "2"},
        ])
    );
}

#[test]
fn tool_choice_is_translated_to_each_protocols_spelling() {
    let with = |protocol, choice: Value| {
        let mut body = tool_transcript();
        body["tool_choice"] = choice;
        translated(protocol, body, None).0
    };
    assert_eq!(
        with(Protocol::Anthropic, json!("required"))["tool_choice"],
        json!({"type": "any"})
    );
    assert_eq!(
        with(
            Protocol::Anthropic,
            json!({"type": "function", "function": {"name": "get_weather"}})
        )["tool_choice"],
        json!({"type": "tool", "name": "get_weather"})
    );
    // `none` is expressed by offering nothing to call, which is exact and does
    // not depend on a form newer than the pinned API version.
    let none = with(Protocol::Anthropic, json!("none"));
    assert!(none.get("tools").is_none() && none.get("tool_choice").is_none());

    assert_eq!(
        with(Protocol::Gemini, json!("required"))["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY"}})
    );
    assert_eq!(
        with(
            Protocol::Gemini,
            json!({"type": "function", "function": {"name": "get_weather"}})
        )["toolConfig"],
        json!({"functionCallingConfig":
            {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}})
    );
}

#[test]
fn anthropic_tool_use_becomes_openai_tool_calls() {
    let body = br#"{"id":"msg_9","content":[
        {"type":"text","text":"Let me check."},
        {"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}],
        "stop_reason":"tool_use","usage":{"input_tokens":9,"output_tokens":4}}"#;
    let (bytes, usage) = translate_response(Protocol::Anthropic, body, &ctx()).unwrap();
    let out: Value = serde_json::from_slice(&bytes).unwrap();
    let message = &out["choices"][0]["message"];
    assert_eq!(message["content"], json!("Let me check."));
    assert_eq!(
        message["tool_calls"],
        json!([{"id": "toolu_1", "type": "function", "function":
            {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]),
        "arguments go back to a string, which is how OpenAI carries them"
    );
    assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
    assert_eq!(usage, Usage::new(9, 4));
}

/// `content` must be null rather than `""` when the model only called tools —
/// clients branch on the distinction.
#[test]
fn a_tool_only_answer_reports_null_content() {
    let body = br#"{"id":"m","content":[{"type":"tool_use","id":"t","name":"f","input":{}}],
        "stop_reason":"tool_use"}"#;
    let (bytes, _) = translate_response(Protocol::Anthropic, body, &ctx()).unwrap();
    let out: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(out["choices"][0]["message"]["content"], Value::Null);
}

/// Gemini identifies a call by name alone, but an OpenAI client pairs the
/// result back by id and sends that id in the next request — so one is
/// invented, deterministically.
#[test]
fn gemini_function_calls_get_stable_synthetic_ids() {
    let body = br#"{"candidates":[{"content":{"parts":[
        {"functionCall":{"name":"get_weather","args":{"city":"Paris"}}}]},
        "finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2}}"#;
    let (bytes, _) = translate_response(Protocol::Gemini, body, &ctx()).unwrap();
    let out: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"],
        json!([{"id": "call_00000000deadbeef_0", "type": "function", "function":
            {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}])
    );
    // Gemini says STOP on the very event that carries the call; reporting
    // "stop" would tell the client the turn was over and the call needed no
    // reply.
    assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
}

/// Reassemble streamed tool calls the way a client does: index identifies the
/// call, and `arguments` fragments concatenate into one JSON string.
fn replay_tool_calls(out: &[u8]) -> Vec<(String, String, String)> {
    let mut calls: Vec<(String, String, String)> = Vec::new();
    for event in SseDecoder::default().push(out) {
        if event.data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data).unwrap();
        let Some(deltas) = chunk["choices"][0]["delta"]["tool_calls"].as_array() else {
            continue;
        };
        for delta in deltas {
            let index = delta["index"].as_u64().unwrap() as usize;
            if index == calls.len() {
                calls.push((
                    delta["id"].as_str().unwrap_or_default().to_string(),
                    delta["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    String::new(),
                ));
            }
            calls[index]
                .2
                .push_str(delta["function"]["arguments"].as_str().unwrap_or_default());
        }
    }
    calls
}

/// The streaming case worth guarding: Anthropic sends the arguments as
/// fragments of JSON that are not valid on their own, and the index it uses
/// counts text blocks too — so it cannot be forwarded as the OpenAI index.
#[test]
fn anthropic_streams_partial_json_into_indexed_tool_call_deltas() {
    const STREAM: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking.\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"Paris\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = t.push(STREAM.as_bytes());
    out.extend(t.finish());

    let (content, finish, done, usage) = replay(&out);
    assert_eq!(content, "Checking.");
    assert_eq!(finish.as_deref(), Some("tool_calls"));
    assert!(done);
    assert_eq!(usage, Some(Usage::new(9, 12)));
    assert_eq!(
        replay_tool_calls(&out),
        vec![(
            "toolu_1".to_string(),
            "get_weather".to_string(),
            // The fragments were forwarded unparsed and only became valid JSON
            // once the client had concatenated them.
            "{\"city\":\"Paris\"}".to_string()
        )],
        "the tool call is index 0 even though Anthropic called its block 1"
    );
}

/// Byte-at-a-time, because the fragments arrive split at arbitrary boundaries
/// and a re-framer that only works on whole events is not the one in
/// production.
#[test]
fn a_streamed_tool_call_survives_being_read_one_byte_at_a_time() {
    const STREAM: &str = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = Vec::new();
    for byte in STREAM.as_bytes() {
        out.extend(t.push(&[*byte]));
    }
    out.extend(t.finish());
    assert_eq!(
        replay_tool_calls(&out),
        vec![("t1".into(), "f".into(), "{\"a\":1}".into())]
    );
}

/// Two calls in one message must land on distinct indices, or the client
/// concatenates both argument lists into the first call.
#[test]
fn parallel_streamed_calls_get_distinct_indices() {
    const STREAM: &str = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"partial_json\":\"{}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"g\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"partial_json\":\"{\\\"x\\\":2}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mut t = StreamTranslator::new(Protocol::Anthropic, ctx()).unwrap();
    let mut out = t.push(STREAM.as_bytes());
    out.extend(t.finish());
    assert_eq!(
        replay_tool_calls(&out),
        vec![
            ("t1".into(), "f".into(), "{}".into()),
            ("t2".into(), "g".into(), "{\"x\":2}".into()),
        ]
    );
}

/// Gemini delivers a call complete in one event, so it goes out as one frame a
/// client can act on immediately rather than being split to look like
/// Anthropic's.
#[test]
fn gemini_streams_a_whole_call_in_one_frame() {
    const STREAM: &str = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"f\",\"args\":{\"x\":1}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":5}}\n\n";
    let mut t = StreamTranslator::new(Protocol::Gemini, ctx()).unwrap();
    let mut out = t.push(STREAM.as_bytes());
    out.extend(t.finish());
    assert_eq!(
        replay_tool_calls(&out),
        vec![(
            "call_00000000deadbeef_0".into(),
            "f".into(),
            "{\"x\":1}".into()
        )]
    );
    let (_, finish, done, _) = replay(&out);
    assert_eq!(finish.as_deref(), Some("tool_calls"));
    assert!(done);
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

/// Order carries meaning — the same words before and after an image ask
/// different questions — so parts keep their sequence rather than being sorted
/// into text-then-media.
#[test]
fn an_inline_image_keeps_its_place_among_the_text() {
    let body = json!({
        "max_tokens": 50,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is wrong with"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KAAA="}},
            {"type": "text", "text": "this?"},
        ]}],
    });
    let (anthropic, _) = translated(Protocol::Anthropic, body.clone(), None);
    assert_eq!(
        anthropic["messages"][0]["content"],
        json!([
            {"type": "text", "text": "What is wrong with"},
            {"type": "image", "source": {"type": "base64",
                "media_type": "image/png", "data": "iVBORw0KAAA="}},
            {"type": "text", "text": "this?"},
        ])
    );

    let (gemini, _) = translated(Protocol::Gemini, body, None);
    assert_eq!(
        gemini["contents"][0]["parts"],
        json!([
            {"text": "What is wrong with"},
            {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KAAA="}},
            {"text": "this?"},
        ])
    );
}

/// The base64 payload must reach the provider byte for byte. Re-encoding it
/// would be work on the request path for no gain, and a single altered
/// character is an image that fails to decode several layers away.
#[test]
fn the_base64_payload_is_never_re_encoded() {
    const DATA: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 5, "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": format!("data:image/gif;base64,{DATA}")}},
        ]}]}),
        None,
    );
    assert_eq!(body["messages"][0]["content"][0]["source"]["data"], DATA);
}

/// A remote URL is never fetched — that would be a network call while serving
/// a request. Anthropic will fetch it itself, so it is expressible there.
#[test]
fn a_remote_image_is_handed_to_anthropic_rather_than_downloaded() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 5, "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.test/x.png"}},
        ]}]}),
        None,
    );
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({"type": "image", "source": {"type": "url", "url": "https://example.test/x.png"}})
    );
}

/// Gemini's `fileData.fileUri` only addresses Google's own Files API, so an
/// arbitrary URL has no expressible form. Naming the fix beats dropping the
/// image and answering a question about a picture the model never saw.
#[test]
fn a_remote_image_is_refused_for_gemini_with_the_fix_named() {
    let err = translate_request(
        Protocol::Gemini,
        json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.test/x.png"}},
        ]}]})
        .to_string()
        .as_bytes(),
        "m",
        Some(10),
    )
    .unwrap_err();
    assert_eq!(
        err,
        TranslateError::Unsupported("a remote image URL for a Gemini backend; send a data: URL")
    );
}

/// Gemini takes audio the same way it takes images; Anthropic has no audio
/// input at all, and an image block with an audio media type would be rejected
/// upstream with a message the client cannot act on.
#[test]
fn audio_reaches_gemini_and_is_refused_by_name_for_anthropic() {
    let body = json!({
        "max_tokens": 5,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Transcribe"},
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
        ]}],
    });
    let (gemini, _) = translated(Protocol::Gemini, body.clone(), None);
    assert_eq!(
        gemini["contents"][0]["parts"][1],
        json!({"inlineData": {"mimeType": "audio/wav", "data": "UklGRg=="}})
    );

    let err = refusal(body);
    assert_eq!(
        err,
        TranslateError::Unsupported("audio input for an Anthropic backend")
    );
}

/// A text-only turn must still go out as a bare string, not a one-element block
/// array — the shape both providers' own clients send, and the one every
/// existing byte-exact assertion in this file depends on.
#[test]
fn adding_media_support_did_not_change_the_text_only_shape() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 5, "messages": [
            {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            {"role": "assistant", "content": "ok"},
        ]}),
        None,
    );
    assert_eq!(body["messages"][0]["content"], json!("ab"));
    assert_eq!(body["messages"][1]["content"], json!("ok"));
}

/// Merging two adjacent user turns must not promote a text-only pair into
/// block form, nor lose the image when one of them carries it.
#[test]
fn merging_turns_preserves_media_and_the_plain_text_shape() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 5, "messages": [
            {"role": "user", "content": "first"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
            ]},
        ]}),
        None,
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "first"},
            {"type": "image", "source": {"type": "base64",
                "media_type": "image/png", "data": "AAA"}},
        ]}])
    );
}

/// A `data:` URL that is not base64 is refused rather than forwarded as a
/// remote URL, which would send the provider a fetch it cannot perform.
#[test]
fn a_non_base64_data_url_does_not_masquerade_as_a_remote_one() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 5, "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png,%89PNG"}},
        ]}]}),
        None,
    );
    // Anthropic can take a URL source, so this is expressible — but it must not
    // have been mistaken for inline base64 and sent as bytes.
    assert_eq!(
        body["messages"][0]["content"][0]["source"]["type"],
        json!("url")
    );
}

// ---------------------------------------------------------------------------
// Structured output and prompt caching
// ---------------------------------------------------------------------------

fn with_schema() -> Value {
    json!({
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "Extract the dates."}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "dates",
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"start": {"type": "string"}},
                    "required": ["start"],
                },
            },
        },
    })
}

/// Anthropic ignores OpenAI's `response_format` outright — sending it through
/// unchanged would silently produce unstructured output, which is worse than
/// the 501 this used to be. The native spelling is `output_config.format`.
#[test]
fn a_json_schema_becomes_anthropics_output_config() {
    let (body, _) = translated(Protocol::Anthropic, with_schema(), None);
    assert_eq!(
        body["output_config"],
        json!({"format": {"type": "json_schema", "schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {"start": {"type": "string"}},
            "required": ["start"],
        }}})
    );
}

#[test]
fn a_json_schema_becomes_geminis_response_schema() {
    let (body, _) = translated(Protocol::Gemini, with_schema(), None);
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        json!("application/json")
    );
    assert_eq!(
        body["generationConfig"]["responseSchema"],
        json!({"type": "object", "properties": {"start": {"type": "string"}},
               "required": ["start"]}),
        "pruned the same way tool schemas are: Gemini rejects additionalProperties"
    );
}

/// `{"type":"json_object"}` asks for *some* JSON with no schema. Gemini has
/// that exactly; Anthropic does not, and inventing an empty schema would
/// constrain the model to `{}` — so it is dropped rather than approximated.
#[test]
fn schemaless_json_mode_maps_only_where_it_exists() {
    let body = json!({
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_object"},
    });

    let (gemini, _) = translated(Protocol::Gemini, body.clone(), None);
    assert_eq!(
        gemini["generationConfig"]["responseMimeType"],
        json!("application/json")
    );
    assert!(gemini["generationConfig"].get("responseSchema").is_none());

    let (anthropic, _) = translated(Protocol::Anthropic, body, None);
    assert!(
        anthropic.get("output_config").is_none(),
        "an empty schema would constrain the model to {{}}: {anthropic}"
    );
}

/// Anthropic caches nothing unless a block asks to be cached, and a hit costs
/// 90% less. An OpenAI-format client cannot express that, so a translated
/// backend was paying full price on every request for a prefix identical
/// across all of them.
#[test]
fn the_system_prompt_carries_a_cache_breakpoint() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({
            "max_tokens": 10,
            "messages": [
                {"role": "system", "content": "You are a careful assistant."},
                {"role": "user", "content": "hi"},
            ],
        }),
        None,
    );
    assert_eq!(
        body["system"],
        json!([{
            "type": "text",
            "text": "You are a careful assistant.",
            "cache_control": {"type": "ephemeral"},
        }])
    );
}

/// The breakpoint goes on the system prompt and nowhere else. A message is not
/// stable across turns, so marking one would be guessing at which prefix
/// repeats — and a misplaced breakpoint costs a cache write for nothing.
#[test]
fn no_cache_breakpoint_is_placed_on_a_message() {
    let (body, _) = translated(
        Protocol::Anthropic,
        json!({"max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}),
        None,
    );
    assert!(body.get("system").is_none(), "no system prompt to mark");
    assert!(
        !body["messages"].to_string().contains("cache_control"),
        "{body}"
    );
}
