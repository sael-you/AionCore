//! OpenAI Realtime session objects (GA field names, verified live in S0-A
//! except `reasoning` and `max_output_tokens` — `max_output_tokens` is echoed by
//! `session.updated` ("inf" default); `reasoning.effort` is per PLAN §3.1 and is
//! checked at the S2 provider gate).

use serde_json::{Value, json};

use super::EngineSessionConfig;

pub(crate) fn turn_detection(config: &EngineSessionConfig) -> Value {
    json!({
        "type": "semantic_vad",
        "eagerness": config.eagerness,
        "create_response": config.create_response,
        "interrupt_response": true,
    })
}

/// Full `session` object for call creation and `session.update` on attach.
pub(crate) fn session_object(config: &EngineSessionConfig) -> Value {
    let mut transcription = json!({
        "model": config.transcription_model,
        "language": "en",
    });
    if !config.keywords.is_empty() {
        transcription["keywords"] = json!(config.keywords);
    }
    json!({
        "type": "realtime",
        "model": config.model,
        "instructions": config.instructions,
        "output_modalities": ["audio"],
        "max_output_tokens": config.max_output_tokens,
        "reasoning": {"effort": config.reasoning_effort},
        "audio": {
            "input": {
                "turn_detection": turn_detection(config),
                "transcription": transcription,
                "noise_reduction": {"type": config.noise_reduction},
            },
            "output": {"voice": config.voice},
        },
        "tools": config.tools,
        "tool_choice": if config.tools.is_empty() { "none" } else { "auto" },
    })
}

/// `session.update` carrying the full config (attach / re-attach).
pub(crate) fn full_update(config: &EngineSessionConfig) -> Value {
    json!({"type": "session.update", "session": session_object(config)})
}

/// Mid-session update: only `turn_detection.create_response` changes (§3.5).
pub(crate) fn create_response_update(config: &EngineSessionConfig) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "audio": {"input": {"turn_detection": turn_detection(config)}},
        },
    })
}

pub(crate) fn response_create(max_output_tokens: u64) -> Value {
    json!({"type": "response.create", "response": {"max_output_tokens": max_output_tokens}})
}

/// Out-of-band exact utterance (S0-A item 4: `conversation: none`, `input: []`,
/// `tool_choice: none`, minimal reasoning).
pub(crate) fn speak_exact(text: &str, max_output_tokens: u64) -> Value {
    json!({
        "type": "response.create",
        "response": {
            "conversation": "none",
            "input": [],
            "tool_choice": "none",
            "max_output_tokens": max_output_tokens,
            "instructions": format!("Say exactly the following text and nothing else: {text}"),
        },
    })
}

pub(crate) fn response_cancel() -> Value {
    json!({"type": "response.cancel"})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EngineSessionConfig {
        EngineSessionConfig {
            model: "gpt-realtime-2.1".into(),
            instructions: "be brief".into(),
            voice: "marin".into(),
            eagerness: "auto".into(),
            noise_reduction: "far_field".into(),
            transcription_model: "gpt-transcribe".into(),
            keywords: vec![],
            reasoning_effort: "low".into(),
            max_output_tokens: 600,
            create_response: true,
            tools: vec![],
        }
    }

    #[test]
    fn session_object_uses_ga_field_names() {
        let session = session_object(&config());
        assert_eq!(session["type"], "realtime");
        assert_eq!(session["audio"]["input"]["turn_detection"]["type"], "semantic_vad");
        assert_eq!(session["audio"]["input"]["turn_detection"]["create_response"], true);
        assert_eq!(session["audio"]["input"]["turn_detection"]["interrupt_response"], true);
        assert_eq!(session["audio"]["input"]["transcription"]["model"], "gpt-transcribe");
        assert!(session["audio"]["input"]["transcription"].get("keywords").is_none());
        assert_eq!(session["audio"]["input"]["noise_reduction"]["type"], "far_field");
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        assert_eq!(session["max_output_tokens"], 600);
        assert_eq!(session["tool_choice"], "none");

        let mut with_keywords = config();
        with_keywords.keywords = vec!["AionCore".into()];
        assert_eq!(
            session_object(&with_keywords)["audio"]["input"]["transcription"]["keywords"][0],
            "AionCore"
        );
    }

    #[test]
    fn mid_session_update_touches_only_turn_detection() {
        let mut cfg = config();
        cfg.create_response = false;
        let update = create_response_update(&cfg);
        let session = update["session"].as_object().unwrap();
        assert_eq!(session.len(), 2, "only type + audio: {session:?}");
        assert!(session.get("instructions").is_none());
        assert!(session.get("tools").is_none());
        assert_eq!(
            update["session"]["audio"]["input"]["turn_detection"]["create_response"],
            false
        );
    }
}
