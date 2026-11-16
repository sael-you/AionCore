//! `aioncore board` — the agent-facing project board CLI, plus the
//! operator-only `board recover`.
//!
//! Agent commands talk to the agent router `/api/runtime/board/…` with the
//! three `x-aionui-*` runtime headers (never a JWT). Flags are selectors; the
//! payload is one JSON object on stdin, validated locally against the
//! registry descriptor before the round trip. Output is the standard
//! envelope (`success`, `data` | `error{code,message,details?}`, `meta`).
//!
//! `board recover` does not need a running server: it opens
//! `<data-dir>/aionui-backend.db` directly (AionUi must be stopped) — see
//! `aionui_board::recover`.

use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

use aionui_api_types::{BoardCliEnvelope, BoardCliError, board_tool_for_cli_command};
use serde_json::{Map, Value, json};

use crate::cli::{BoardArgs, BoardCommand, BoardListArgs, Cli};
use crate::commands::board_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

const SCHEMA_VALIDATION_FAILED: &str = "schema_validation_failed";
const TRANSPORT_UNAVAILABLE: &str = "transport_unavailable";

pub(crate) async fn run_board(cli: &Cli, args: &BoardArgs) -> ExitCode {
    match run_board_inner(cli, args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// One HTTP call to the agent router.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoardRequest {
    pub command: &'static str,
    pub method: reqwest::Method,
    /// Path below `/api/runtime/board`.
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

async fn run_board_inner(cli: &Cli, args: &BoardArgs) -> Result<(), ExitCode> {
    match &args.command {
        // Static contract; needs no runtime env.
        BoardCommand::Capabilities => print_json(&BoardCliEnvelope::success(
            board_capabilities::data(),
            "board capabilities",
        )),
        BoardCommand::Recover(recover) => recover_command(cli, recover).await,
        BoardCommand::Unknown(path) => {
            let suffix = path
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            Err(print_failure(
                format!("board {suffix}").trim_end(),
                "BOARD_CLI_UNKNOWN_COMMAND",
                SCHEMA_VALIDATION_FAILED,
                "unknown board command; run `aioncore board capabilities`".into(),
                None,
            ))
        }
        command => {
            let mut stdin = read_stdin_if_needed(command)?;
            // `update N --assignee-conversation X` alone: read the version first.
            if let BoardCommand::Update(update) = command
                && update.assignee_conversation.is_some()
                && stdin.get("version").is_none()
                && let Value::Object(object) = &mut stdin
            {
                object.insert("version".into(), json!(card_version(&update.card).await?));
            }
            let request = build_request(command, stdin)?;
            send(request).await
        }
    }
}

/// Which descriptor validates a command's stdin (None = reads no stdin).
fn stdin_tool(command: &BoardCommand) -> Option<&'static str> {
    match command {
        BoardCommand::Create => Some("create"),
        BoardCommand::Update(_) => Some("update"),
        BoardCommand::Move(_) => Some("move"),
        BoardCommand::Review(_) => Some("review"),
        BoardCommand::Comment(_) => Some("comment"),
        _ => None,
    }
}

fn command_name(command: &BoardCommand) -> &'static str {
    match command {
        BoardCommand::Capabilities => "board capabilities",
        BoardCommand::Status => "board status",
        BoardCommand::List(_) => "board list",
        BoardCommand::Show(_) => "board show",
        BoardCommand::Create => "board create",
        BoardCommand::Update(_) => "board update",
        BoardCommand::Move(_) => "board move",
        BoardCommand::Review(_) => "board review",
        BoardCommand::Comment(_) => "board comment",
        BoardCommand::Block(_) => "board block",
        BoardCommand::Unblock(_) => "board unblock",
        BoardCommand::Link(_) => "board link",
        BoardCommand::Events(_) => "board events",
        BoardCommand::Recover(_) => "board recover",
        BoardCommand::Unknown(_) => "board",
    }
}

/// Read the stdin object of a payload command. An interactive terminal or
/// empty stdin is `{}` (the server or the schema then reports what is
/// missing).
fn read_stdin_if_needed(command: &BoardCommand) -> Result<Value, ExitCode> {
    let name = command_name(command);
    if stdin_tool(command).is_none() || io::stdin().is_terminal() {
        return Ok(json!({}));
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|error| {
        print_failure(
            name,
            "BOARD_CLI_STDIN_READ_FAILED",
            SCHEMA_VALIDATION_FAILED,
            error.to_string(),
            None,
        )
    })?;
    if input.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&input).map_err(|error| {
        print_failure(
            name,
            "BOARD_CLI_STDIN_JSON_INVALID",
            SCHEMA_VALIDATION_FAILED,
            error.to_string(),
            None,
        )
    })
}

/// Validate stdin against the registry descriptor: an object, no unknown
/// keys, every required key present.
pub(crate) fn validate_stdin(command: &'static str, tool: &str, value: Value) -> Result<Map<String, Value>, String> {
    let Value::Object(object) = value else {
        return Err(format!("{command}: stdin JSON must be an object"));
    };
    let descriptor = board_tool_for_cli_command(tool).ok_or_else(|| format!("no descriptor for {tool}"))?;
    let properties = descriptor.input_schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(unknown) = object.keys().find(|key| !properties.contains_key(*key)) {
        return Err(format!("unknown stdin field: {unknown}"));
    }
    if let Some(required) = descriptor.input_schema["required"].as_array()
        && let Some(missing) = required
            .iter()
            .filter_map(Value::as_str)
            .find(|key| !object.contains_key(*key))
    {
        return Err(format!("missing required stdin field: {missing}"));
    }
    Ok(object)
}

fn list_query(args: &BoardListArgs) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    let mut push = |key: &'static str, value: Option<String>| {
        if let Some(value) = value {
            query.push((key, value));
        }
    };
    push("column", args.column.clone());
    push("assignee", args.assignee.clone());
    push("type", args.card_type.clone());
    push("label", args.label.clone());
    push("skill", args.skill.clone());
    push("priority", args.priority.clone());
    push("blocked", args.blocked.map(|b| b.to_string()));
    push("include_archived", args.include_archived.then(|| "true".to_owned()));
    push("limit", args.limit.map(|l| l.to_string()));
    push("cursor", args.cursor.clone());
    query
}

/// Map a parsed command (+ its stdin object) to the agent-router call.
pub(crate) fn build_request(command: &BoardCommand, stdin: Value) -> Result<BoardRequest, ExitCode> {
    let name = command_name(command);
    let schema_failure = |message: String| {
        print_failure(
            name,
            "BOARD_CLI_SCHEMA_VALIDATION_FAILED",
            SCHEMA_VALIDATION_FAILED,
            message,
            None,
        )
    };
    let payload = match stdin_tool(command) {
        Some(tool) => Some(validate_stdin(name, tool, stdin).map_err(schema_failure)?),
        None => None,
    };
    let card_path = |card: &str, suffix: &str| format!("/cards/{}{suffix}", encode_segment(card.trim()));
    let get = |path: String, query: Vec<(&'static str, String)>| BoardRequest {
        command: name,
        method: reqwest::Method::GET,
        path,
        query,
        body: None,
    };
    let write = |method: reqwest::Method, path: String, body: Value| BoardRequest {
        command: name,
        method,
        path,
        query: Vec::new(),
        body: Some(body),
    };
    Ok(match command {
        BoardCommand::Status => get("/status".into(), Vec::new()),
        BoardCommand::List(args) => get("/cards".into(), list_query(args)),
        BoardCommand::Show(args) => get(
            card_path(&args.card, ""),
            if args.packet {
                vec![("packet", "true".into())]
            } else {
                Vec::new()
            },
        ),
        BoardCommand::Create => write(
            reqwest::Method::POST,
            "/cards".into(),
            Value::Object(payload.unwrap_or_default()),
        ),
        BoardCommand::Update(args) => {
            let mut body = payload.unwrap_or_default();
            if let Some(conversation) = &args.assignee_conversation {
                body.insert("assignee_conversation".into(), json!(conversation));
            }
            write(reqwest::Method::PATCH, card_path(&args.card, ""), Value::Object(body))
        }
        BoardCommand::Move(args) => {
            let mut body = payload.unwrap_or_default();
            body.insert("to_column".into(), json!(args.column));
            if let Some(note) = &args.approval_note {
                body.insert("approval_note".into(), json!(note));
            }
            write(
                reqwest::Method::POST,
                card_path(&args.card, "/move"),
                Value::Object(body),
            )
        }
        BoardCommand::Review(args) => write(
            reqwest::Method::PUT,
            card_path(
                &args.card,
                &format!("/reviews/{}", encode_segment(args.department.trim())),
            ),
            Value::Object(payload.unwrap_or_default()),
        ),
        BoardCommand::Comment(args) => {
            let mut body = payload.unwrap_or_default();
            if let Some(to) = &args.to_department {
                body.insert("to_department".into(), json!(to));
            }
            if let Some(answers) = &args.answers {
                body.insert("answers".into(), json!(answers));
            }
            write(
                reqwest::Method::POST,
                card_path(&args.card, "/comments"),
                Value::Object(body),
            )
        }
        BoardCommand::Block(args) => write(
            reqwest::Method::POST,
            card_path(&args.card, "/block"),
            json!({ "reason": args.reason }),
        ),
        BoardCommand::Unblock(args) => write(reqwest::Method::POST, card_path(&args.card, "/unblock"), json!({})),
        BoardCommand::Link(args) => {
            let body = match &args.role {
                Some(role) => json!({ "role": role }),
                None => json!({}),
            };
            write(reqwest::Method::POST, card_path(&args.card, "/links"), body)
        }
        BoardCommand::Events(args) => {
            let mut query = Vec::new();
            if let Some(after) = args.after_seq {
                query.push(("after_seq", after.to_string()));
            }
            if let Some(limit) = args.limit {
                query.push(("limit", limit.to_string()));
            }
            get("/events".into(), query)
        }
        BoardCommand::Capabilities | BoardCommand::Recover(_) | BoardCommand::Unknown(_) => {
            return Err(schema_failure("not an agent-router command".into()));
        }
    })
}

/// Percent-encode one URL path segment (everything but RFC 3986 unreserved).
pub(crate) fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    fn from_env(command: &str) -> Result<Self, ExitCode> {
        let required = |name: &'static str| {
            std::env::var(name).map_err(|_| {
                print_failure(
                    command,
                    "BOARD_CLI_ENV_MISSING",
                    TRANSPORT_UNAVAILABLE,
                    format!("missing required environment variable: {name}"),
                    None,
                )
            })
        };
        Ok(Self {
            base_url: required(ENV_BASE_URL)?,
            user_id: required(ENV_USER_ID)?,
            conversation_id: required(ENV_CONVERSATION_ID)?,
            runtime_token: required(ENV_RUNTIME_TOKEN)?,
        })
    }

    /// Fallible: a malformed `AIONUI_*` value comes back as an envelope.
    fn headers(&self, command: &str) -> Result<reqwest::header::HeaderMap, ExitCode> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-aionui-user-id", &self.user_id),
            ("x-aionui-conversation-id", &self.conversation_id),
            ("x-aionui-runtime-token", &self.runtime_token),
        ] {
            let parsed = value.parse().map_err(|_| {
                // The NAME only — a token must never reach stdout or stderr.
                print_failure(
                    command,
                    "BOARD_CLI_HEADER_INVALID",
                    TRANSPORT_UNAVAILABLE,
                    format!("environment variable for {name} is not a valid header value"),
                    None,
                )
            })?;
            headers.insert(name, parsed);
        }
        Ok(headers)
    }
}

/// The card's current version (`board show`), for an update given only flags.
async fn card_version(card: &str) -> Result<i64, ExitCode> {
    const COMMAND: &str = "board update";
    let env = RuntimeEnv::from_env(COMMAND)?;
    let url = format!(
        "{}/api/runtime/board/cards/{}",
        env.base_url.trim_end_matches('/'),
        encode_segment(card.trim())
    );
    let failure = |message: String| {
        print_failure(
            COMMAND,
            "BOARD_CLI_HTTP_BRIDGE_FAILED",
            TRANSPORT_UNAVAILABLE,
            message,
            None,
        )
    };
    let response = reqwest::Client::new()
        .get(url)
        .headers(env.headers(COMMAND)?)
        .send()
        .await
        .map_err(|error| failure(error.to_string()))?;
    let body: Value = response.json().await.map_err(|error| failure(error.to_string()))?;
    body["data"]["card"]["version"]
        .as_i64()
        .ok_or_else(|| failure(format!("could not read the card version: {}", body["error"])))
}

async fn send(request: BoardRequest) -> Result<(), ExitCode> {
    let command = request.command;
    let env = RuntimeEnv::from_env(command)?;
    let url = format!(
        "{}/api/runtime/board{}",
        env.base_url.trim_end_matches('/'),
        request.path
    );
    let mut builder = reqwest::Client::new()
        .request(request.method.clone(), url)
        .headers(env.headers(command)?)
        .query(&request.query);
    if let Some(body) = &request.body {
        builder = builder.json(body);
    }
    let response = builder.send().await.map_err(|error| {
        print_failure(
            command,
            "BOARD_CLI_HTTP_BRIDGE_FAILED",
            TRANSPORT_UNAVAILABLE,
            error.to_string(),
            None,
        )
    })?;
    let status = response.status();
    let text = response.text().await.map_err(|error| {
        print_failure(
            command,
            "BOARD_CLI_HTTP_RESPONSE_FAILED",
            TRANSPORT_UNAVAILABLE,
            error.to_string(),
            None,
        )
    })?;
    println!("{text}");
    if status.is_success() {
        Ok(())
    } else {
        eprintln!("BOARD_CLI_HTTP_STATUS_ERROR command={command} status={status}: board router returned an error");
        Err(ExitCode::from(3))
    }
}

async fn recover_command(cli: &Cli, args: &crate::cli::BoardRecoverArgs) -> Result<(), ExitCode> {
    const COMMAND: &str = "board recover";
    let options = aionui_board::RecoverOptions {
        database: cli.data_dir.join("aionui-backend.db"),
        kept: args.from.clone(),
        dry_run: args.dry_run,
        marker: aionui_board::default_marker_path(&cli.data_dir),
    };
    match aionui_board::recover_database(&options).await {
        Ok(outcome) => print_json(&BoardCliEnvelope::success(outcome, COMMAND)),
        Err(error) => Err(print_failure(
            COMMAND,
            "BOARD_RECOVER_FAILED",
            error.code(),
            error.to_string(),
            None,
        )),
    }
}

fn print_failure(
    command: &str,
    stderr_code: &'static str,
    code: &str,
    message: String,
    details: Option<Value>,
) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {message}");
    let _ = print_json(&BoardCliEnvelope::<Value>::failure(
        BoardCliError {
            code: code.to_owned(),
            message,
            details,
        },
        command,
    ));
    ExitCode::from(2)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), ExitCode> {
    let rendered = serde_json::to_string_pretty(value).map_err(|_| ExitCode::from(1))?;
    let mut stdout = io::stdout();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> BoardCommand {
        let mut argv = vec!["aioncore", "board"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).expect("parses").command {
            Some(crate::cli::Command::Board(board)) => board.command,
            other => panic!("not a board command: {other:?}"),
        }
    }

    #[test]
    fn selectors_are_flags_and_positionals() {
        let BoardCommand::List(list) = parse(&["list", "--column", "todo", "--assignee", "me", "--limit", "5"]) else {
            panic!("list");
        };
        assert_eq!(list.column.as_deref(), Some("todo"));
        let request = build_request(&BoardCommand::List(list), json!({})).unwrap();
        assert_eq!(request.method, reqwest::Method::GET);
        assert_eq!(request.path, "/cards");
        assert_eq!(
            request.query,
            vec![
                ("column", "todo".into()),
                ("assignee", "me".into()),
                ("limit", "5".into())
            ]
        );

        let show = parse(&["show", "TASK-3", "--packet"]);
        let request = build_request(&show, json!({})).unwrap();
        assert_eq!(request.path, "/cards/3", "selectors are normalised to N");
        assert_eq!(request.query, vec![("packet", "true".into())]);
    }

    #[test]
    fn payload_commands_take_their_body_from_stdin() {
        let request = build_request(&parse(&["create"]), json!({ "title": "Fix login" })).unwrap();
        assert_eq!(request.method, reqwest::Method::POST);
        assert_eq!(request.body, Some(json!({ "title": "Fix login" })));

        let request = build_request(&parse(&["update", "4"]), json!({ "version": 2, "priority": "high" })).unwrap();
        assert_eq!(request.method, reqwest::Method::PATCH);
        assert_eq!(request.path, "/cards/4");

        let request = build_request(&parse(&["move", "4", "done"]), json!({ "after": 2 })).unwrap();
        assert_eq!(request.path, "/cards/4/move");
        assert_eq!(request.body, Some(json!({ "after": 2, "to_column": "done" })));

        // TASK-116: the gate approval note and the conversation assignee.
        let request = build_request(
            &parse(&["move", "4", "planned", "--approval-note", "owner said go in chat"]),
            json!({}),
        )
        .unwrap();
        assert_eq!(
            request.body,
            Some(json!({ "to_column": "planned", "approval_note": "owner said go in chat" }))
        );
        let request = build_request(
            &parse(&["update", "TASK-4", "--assignee-conversation", "conv-9"]),
            json!({ "version": 3 }),
        )
        .unwrap();
        assert_eq!(request.path, "/cards/4");
        assert_eq!(
            request.body,
            Some(json!({ "version": 3, "assignee_conversation": "conv-9" }))
        );
        // Both are also accepted on stdin (the descriptors list them).
        assert!(validate_stdin("board move", "move", json!({ "approval_note": "x" })).is_ok());
        assert!(
            validate_stdin(
                "board update",
                "update",
                json!({ "version": 1, "assignee_conversation": "c" })
            )
            .is_ok()
        );
        assert!(
            validate_stdin(
                "board create",
                "create",
                json!({ "title": "t", "assignee_conversation": "c" })
            )
            .is_err()
        );

        let request = build_request(
            &parse(&["comment", "4", "--to", "qa", "--answers", "q1"]),
            json!({ "body": "done" }),
        )
        .unwrap();
        assert_eq!(
            request.body,
            Some(json!({ "body": "done", "to_department": "qa", "answers": "q1" }))
        );

        let request = build_request(&parse(&["block", "4", "--reason", "waits for API"]), json!({})).unwrap();
        assert_eq!(request.body, Some(json!({ "reason": "waits for API" })));
        let request = build_request(&parse(&["link", "4", "--role", "qa"]), json!({})).unwrap();
        assert_eq!(request.body, Some(json!({ "role": "qa" })));
    }

    #[test]
    fn stdin_is_validated_against_the_descriptor() {
        assert!(validate_stdin("board create", "create", json!({ "description": "x" })).is_err());
        assert!(validate_stdin("board create", "create", json!({ "title": "x", "number": 9 })).is_err());
        assert!(validate_stdin("board update", "update", json!({ "title": "x" })).is_err());
        assert!(validate_stdin("board create", "create", json!(["title"])).is_err());
        assert!(validate_stdin("board move", "move", json!({})).is_ok());
        assert!(
            validate_stdin("board update", "update", json!({ "version": 1, "blocked": true })).is_err(),
            "blocking goes through the block command"
        );
    }

    #[test]
    fn recover_takes_a_source_and_an_optional_dry_run() {
        let BoardCommand::Recover(args) = parse(&["recover", "--from", "/tmp/kept.db", "--dry-run"]) else {
            panic!("recover");
        };
        assert_eq!(args.from, std::path::PathBuf::from("/tmp/kept.db"));
        assert!(args.dry_run);
        let mut argv = vec!["aioncore", "board", "recover"];
        argv.push("--dry-run");
        assert!(Cli::try_parse_from(argv).is_err(), "--from is required");
    }

    #[test]
    fn card_selectors_are_validated_and_segments_encoded() {
        for bad in ["0", "TASK-", "4/../../x", "4?x=1", "-3", "TASK-1a"] {
            assert!(
                Cli::try_parse_from(["aioncore", "board", "show", bad]).is_err(),
                "{bad}"
            );
        }
        let request = build_request(&parse(&["review", "4", "q a/../x"]), json!({ "verdict": "approve" })).unwrap();
        assert_eq!(request.path, "/cards/4/reviews/q%20a%2F..%2Fx");
    }

    #[test]
    fn block_requires_a_reason() {
        assert!(Cli::try_parse_from(["aioncore", "board", "block", "4"]).is_err());
    }

    #[test]
    fn review_puts_the_verdict_for_the_department() {
        let request = build_request(&parse(&["review", "TASK-4", "qa"]), json!({ "verdict": "approve" })).unwrap();
        assert_eq!(request.method, reqwest::Method::PUT);
        assert_eq!(request.path, "/cards/4/reviews/qa");
        assert_eq!(request.body, Some(json!({ "verdict": "approve" })));
        assert!(
            validate_stdin("board review", "review", json!({ "summary": "x" })).is_err(),
            "verdict required"
        );
    }

    #[test]
    fn unknown_commands_are_captured_not_rejected_by_clap() {
        assert!(matches!(parse(&["gate", "4", "approve"]), BoardCommand::Unknown(_)));
    }
}
