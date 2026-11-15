//! OpenAI Realtime server events → engine-neutral [`EngineEvent`]s.
//! Shapes follow events captured live in S0-A (`/root/.clauterm/tmp/s0-openai/out`).

use serde_json::Value;

use super::{EngineEvent, TranscriptRole};
use crate::pricing::Usage;

fn str_field(event: &Value, key: &str) -> Option<String> {
    event.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u64_at(value: &Value, path: &[&str]) -> u64 {
    path.iter()
        .try_fold(value, |v, key| v.get(*key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Parse a `response.usage` block; `None` when absent or null.
pub(crate) fn parse_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    Some(Usage {
        input_tokens: u64_at(usage, &["input_tokens"]),
        output_tokens: u64_at(usage, &["output_tokens"]),
        input_text: u64_at(usage, &["input_token_details", "text_tokens"]),
        input_audio: u64_at(usage, &["input_token_details", "audio_tokens"]),
        cached_text: u64_at(usage, &["input_token_details", "cached_tokens_details", "text_tokens"]),
        cached_audio: u64_at(usage, &["input_token_details", "cached_tokens_details", "audio_tokens"]),
        output_text: u64_at(usage, &["output_token_details", "text_tokens"]),
        output_audio: u64_at(usage, &["output_token_details", "audio_tokens"]),
    })
}

/// Map one server event. `None` for events voice does not use.
pub(crate) fn map_event(event: &Value) -> Option<EngineEvent> {
    let kind = event.get("type")?.as_str()?;
    let mapped = match kind {
        "input_audio_buffer.speech_started" => EngineEvent::UserSpeechStarted {
            audio_start_ms: event.get("audio_start_ms").and_then(Value::as_u64),
        },
        "input_audio_buffer.speech_stopped" => EngineEvent::UserSpeechStopped {
            audio_end_ms: event.get("audio_end_ms").and_then(Value::as_u64),
        },
        "input_audio_buffer.committed" => EngineEvent::InputCommitted {
            item_id: str_field(event, "item_id"),
        },
        "conversation.item.input_audio_transcription.delta" => EngineEvent::Transcript {
            role: TranscriptRole::User,
            text: str_field(event, "delta")?,
            is_final: false,
            stt_seconds: None,
        },
        "conversation.item.input_audio_transcription.completed" => EngineEvent::Transcript {
            role: TranscriptRole::User,
            text: str_field(event, "transcript").unwrap_or_default(),
            is_final: true,
            stt_seconds: event
                .get("usage")
                .filter(|u| u.get("type").and_then(Value::as_str) == Some("duration"))
                .and_then(|u| u.get("seconds"))
                .and_then(|s| s.as_f64())
                .map(|s| s.ceil() as u64),
        },
        "response.output_audio_transcript.delta" => EngineEvent::Transcript {
            role: TranscriptRole::Assistant,
            text: str_field(event, "delta")?,
            is_final: false,
            stt_seconds: None,
        },
        "response.output_audio_transcript.done" => EngineEvent::Transcript {
            role: TranscriptRole::Assistant,
            text: str_field(event, "transcript").unwrap_or_default(),
            is_final: true,
            stt_seconds: None,
        },
        "response.created" => EngineEvent::ResponseStarted {
            response_id: event.get("response")?.get("id")?.as_str()?.to_owned(),
        },
        "response.done" => {
            let response = event.get("response")?;
            EngineEvent::ResponseDone {
                response_id: response.get("id")?.as_str()?.to_owned(),
                status: str_field(response, "status").unwrap_or_else(|| "unknown".into()),
                usage: response.get("usage").and_then(parse_usage),
            }
        }
        "output_audio_buffer.started" => EngineEvent::AssistantAudioStarted {
            response_id: str_field(event, "response_id"),
        },
        "output_audio_buffer.stopped" => EngineEvent::AssistantAudioFinished {
            response_id: str_field(event, "response_id")?,
        },
        "response.function_call_arguments.done" => EngineEvent::ToolCall {
            call_id: str_field(event, "call_id")?,
            name: str_field(event, "name")?,
            arguments: str_field(event, "arguments").unwrap_or_default(),
        },
        "session.updated" => EngineEvent::ConfigApplied,
        "error" => EngineEvent::ProviderError {
            code: event
                .get("error")
                .and_then(|e| e.get("code").or_else(|| e.get("type")))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        },
        _ => return None,
    };
    Some(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_captured_response_done_with_usage() {
        let event = json!({"type": "response.done", "event_id": "event_x", "response": {
            "object": "realtime.response", "id": "resp_1", "status": "completed",
            "usage": {"total_tokens": 519, "input_tokens": 356, "output_tokens": 163,
                "input_token_details": {"text_tokens": 356, "audio_tokens": 0, "image_tokens": 0,
                    "cached_tokens": 0, "cached_tokens_details": {"text_tokens": 0, "audio_tokens": 0, "image_tokens": 0}},
                "output_token_details": {"text_tokens": 90, "audio_tokens": 73, "reasoning_tokens": 64}}}});
        match map_event(&event).unwrap() {
            EngineEvent::ResponseDone {
                response_id,
                status,
                usage,
            } => {
                assert_eq!(response_id, "resp_1");
                assert_eq!(status, "completed");
                let usage = usage.unwrap();
                assert_eq!(usage.input_text, 356);
                assert_eq!(usage.output_audio, 73);
                assert_eq!(usage.output_tokens, 163);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn response_done_with_null_usage_has_none() {
        let event = json!({"type": "response.done", "response": {"id": "r", "status": "cancelled", "usage": null}});
        assert!(matches!(
            map_event(&event),
            Some(EngineEvent::ResponseDone { usage: None, .. })
        ));
    }

    #[test]
    fn maps_vad_transcripts_audio_buffer_and_tools() {
        assert_eq!(
            map_event(&json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 10924})),
            Some(EngineEvent::UserSpeechStarted {
                audio_start_ms: Some(10924)
            })
        );
        assert_eq!(
            map_event(&json!({"type": "conversation.item.input_audio_transcription.completed",
                "transcript": "Please list my projects.", "usage": {"type": "duration", "seconds": 3}})),
            Some(EngineEvent::Transcript {
                role: TranscriptRole::User,
                text: "Please list my projects.".into(),
                is_final: true,
                stt_seconds: Some(3),
            })
        );
        assert_eq!(
            map_event(&json!({"type": "output_audio_buffer.stopped", "response_id": "resp_1"})),
            Some(EngineEvent::AssistantAudioFinished {
                response_id: "resp_1".into()
            })
        );
        assert_eq!(
            map_event(
                &json!({"type": "response.function_call_arguments.done", "call_id": "call_1",
                "name": "list_projects", "arguments": "{}"})
            ),
            Some(EngineEvent::ToolCall {
                call_id: "call_1".into(),
                name: "list_projects".into(),
                arguments: "{}".into()
            })
        );
        assert_eq!(
            map_event(&json!({"type": "error", "error": {"type": "invalid_request_error", "code": "bad"}})),
            Some(EngineEvent::ProviderError { code: "bad".into() })
        );
        assert_eq!(
            map_event(&json!({"type": "response.output_audio.delta", "delta": "AAA"})),
            None
        );
        assert_eq!(map_event(&json!({"no": "type"})), None);
    }
}
