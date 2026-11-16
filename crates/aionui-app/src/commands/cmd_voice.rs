//! `aioncore voice` — agent → user voice (card C9, PLAN v15 §3.4) and the
//! deploy script's live-call check.
//!
//! `say` / `ask` / `wait` call the runtime-token voice routes with the
//! `AIONUI_*` environment the backend injects into agent processes (same
//! shape as `aioncore session`); a voice "yes" is never permission for a
//! destructive action. `status` needs no server: it reads the live-call
//! count from `--data-dir` read-only.

use std::io::{self, Read};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::cli::{Cli, VoiceArgs, VoiceCommand};

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_voice(cli: &Cli, args: &VoiceArgs) -> ExitCode {
    let result = match &args.command {
        VoiceCommand::Capabilities => print(&capabilities()),
        VoiceCommand::Say => call("voice say", "POST", "/api/runtime/voice/say", true).await,
        VoiceCommand::Ask => call("voice ask", "POST", "/api/runtime/voice/ask", true).await,
        VoiceCommand::Wait { id } => {
            let path = format!("/api/runtime/voice/ask/{}", urlencode(id));
            call("voice wait", "GET", &path, false).await
        }
        VoiceCommand::Status => status(cli).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn capabilities() -> Value {
    json!({"success": true, "data": capabilities_data(), "meta": {"schema_version": 1}})
}

/// The voice CLI contract (also checked by the top-level capability index).
pub(crate) fn capabilities_data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-voice-cli",
        "commands": {
            "say": {"stdin": {"text": "string (≤ 500 chars, no confirmation words; in conversation voice mode ≤ 1200 chars, spoken in ≤ 300-char clips)"},
                    "returns": {"status": "delivered | channel | no_device"}},
            "ask": {"stdin": {"question": "string", "timeout_s": "integer, optional (≤ 50)"},
                    "returns": {"status": "answered | pending | timeout | channel | no_device | cancelled", "id": "string",
                                "answer": "string"}},
            "wait": {"args": "<id>", "returns": "same as ask"},
            "status": {"returns": {"live_calls": "integer (all users)"}},
        },
        "max_wait_s": 50,
        "say_per_minute": 6,
        "voice_mode": "GET /api/runtime/voice/capabilities answers voice_mode on|off for the calling conversation; messages spoken in voice mode carry an [[AION_VOICE]] block (skill voice-mode)",
        "rules": [
            "Only conversations the user sent a voice request to (or opted in) may speak.",
            "In conversation voice mode the conversation's own agent speaks without a name prefix and decides itself when it is worth it.",
            "A spoken yes is never a typed approval for production data, payments, ads or email writes.",
            "A voice answer is never permission for a destructive action; ask for an approval instead.",
        ],
    })
}

fn print(value: &Value) -> Result<(), ExitCode> {
    println!("{value}");
    Ok(())
}

fn fail(command: &str, code: &str, message: &str, exit: u8) -> ExitCode {
    eprintln!("{code} command={command}: {message}");
    println!("{}", json!({"success": false, "code": code, "error": message}));
    ExitCode::from(exit)
}

fn env(command: &str, name: &'static str) -> Result<String, ExitCode> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            fail(
                command,
                "VOICE_CLI_ENV_MISSING",
                &format!("missing environment variable {name}"),
                2,
            )
        })
}

async fn call(command: &str, method: &str, path: &str, with_body: bool) -> Result<(), ExitCode> {
    let base = env(command, ENV_BASE_URL)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (header, var) in [
        ("x-aionui-user-id", ENV_USER_ID),
        ("x-aionui-conversation-id", ENV_CONVERSATION_ID),
        ("x-aionui-runtime-token", ENV_RUNTIME_TOKEN),
    ] {
        let value = env(command, var)?;
        // The variable NAME only — a token never reaches stdout or stderr.
        let parsed = value.parse().map_err(|_| {
            fail(
                command,
                "VOICE_CLI_HEADER_INVALID",
                &format!("{var} is not a valid header value"),
                2,
            )
        })?;
        headers.insert(header, parsed);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(65))
        .build()
        .map_err(|_| fail(command, "VOICE_CLI_HTTP_BRIDGE_FAILED", "http client unavailable", 3))?;
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let request = if method == "POST" {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|_| fail(command, "VOICE_CLI_STDIN_READ_FAILED", "could not read stdin", 2))?;
        let body: Value = serde_json::from_str(if input.trim().is_empty() { "{}" } else { &input })
            .map_err(|_| fail(command, "VOICE_CLI_STDIN_JSON_INVALID", "stdin is not a JSON object", 2))?;
        if with_body && !body.is_object() {
            return Err(fail(
                command,
                "VOICE_CLI_STDIN_JSON_INVALID",
                "stdin is not a JSON object",
                2,
            ));
        }
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    let response = request
        .headers(headers)
        .send()
        .await
        .map_err(|_| fail(command, "VOICE_CLI_HTTP_BRIDGE_FAILED", "AionCore is not reachable", 3))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    println!("{text}");
    if status.is_success() {
        Ok(())
    } else {
        eprintln!("VOICE_CLI_HTTP_STATUS_ERROR command={command} status={status}");
        Err(ExitCode::from(3))
    }
}

async fn status(cli: &Cli) -> Result<(), ExitCode> {
    let path = cli.data_dir.join("aionui-backend.db");
    match aionui_db::count_live_voice_calls_read_only(&path).await {
        Ok(count) => print(&json!({
            "success": true,
            "data": {"live_calls": count.unwrap_or(0), "voice_installed": count.is_some()},
        })),
        Err(_) => Err(fail(
            "voice status",
            "VOICE_STATUS_DB_UNAVAILABLE",
            "database not readable",
            3,
        )),
    }
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_name_every_command() {
        let caps = capabilities();
        for name in ["say", "ask", "wait", "status"] {
            assert!(caps["data"]["commands"].get(name).is_some(), "{name}");
        }
        assert_eq!(caps["data"]["max_wait_s"], 50);
    }

    #[test]
    fn ids_are_path_encoded() {
        assert_eq!(urlencode("ask_01/../x"), "ask_01%2F..%2Fx");
    }
}
