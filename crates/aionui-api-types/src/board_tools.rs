//! Agent-facing contract of the `aioncore board` CLI (SPEC §7).
//!
//! The descriptor registry is the single source of truth for `board
//! capabilities` and for the CLI's local stdin validation. Flags are
//! selectors only (card number, filters, column, department, role); every
//! payload is one JSON object on stdin.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON schema of the stdin object (`{}` when the command reads none).
    pub input_schema: Value,
    pub cli_command: Vec<String>,
    /// Selector flags/positionals the command takes.
    pub selectors: Vec<String>,
    pub when: String,
    pub input_summary: String,
    /// Whether the command reads a JSON object from stdin.
    pub reads_stdin: bool,
}

struct Spec {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    cli_command: &'static [&'static str],
    selectors: &'static [&'static str],
    when: &'static str,
    input_summary: &'static str,
    reads_stdin: bool,
}

fn string_array(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn card_fields(required_version: bool) -> Value {
    let mut properties = json!({
        "title": { "type": "string", "maxLength": 200 },
        "description": { "type": "string", "maxLength": 20000, "description": "Markdown." },
        "type": { "enum": ["idea", "bug", "feature", "update", "chore", "research"] },
        "priority": { "enum": ["low", "medium", "high", "urgent"] },
        "size": { "enum": ["s", "m", "l", null] },
        "tier": { "enum": ["t0", "t1", "t2", "t3", null] },
        "labels": string_array("At most 20 labels of at most 40 characters."),
        "skills": string_array("Tagged skill names (at most 20)."),
        "routing": string_array("Department names (at most 10)."),
        "metadata": { "type": "object", "description": "At most 8 KB." },
        "change_id": { "type": ["string", "null"] }
    });
    if required_version {
        properties["version"] = json!({ "type": "integer", "description": "The card version you read (board show)." });
    }
    properties
}

/// `board_update`: the card fields plus the conversation assignee (TASK-116).
fn update_fields() -> Value {
    let mut properties = card_fields(true);
    properties["assignee_conversation"] = json!({
        "type": "string",
        "description": "Assign the card to this conversation id of yours (assignee_kind = conversation)."
    });
    properties
}

fn specs() -> Vec<Spec> {
    let empty = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let mut create_props = card_fields(false);
    create_props["column"] = json!({ "type": "string", "description": "Column key; default = the first column (backlog). Agents may use only the first column: leave it out (the Board Keeper may name a non-gate column)." });
    vec![
        Spec {
            name: "board_status",
            description: "Whether this conversation's project has a board, its template and its Board Keeper (Butler) conversation id. Always answers.",
            input_schema: empty.clone(),
            cli_command: &["status"],
            selectors: &[],
            when: "Only when the user, a card packet or a TASK-N reference asks you to use the board.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
        Spec {
            name: "board_list",
            description: "List cards of this project's board (paged, ordered by column, position).",
            input_schema: empty.clone(),
            cli_command: &["list"],
            selectors: &[
                "--column <key>",
                "--assignee me|<id>",
                "--type <type>",
                "--label <label>",
                "--skill <skill>",
                "--priority <priority>",
                "--blocked true|false",
                "--include-archived",
                "--limit <n>",
                "--cursor <next_cursor>",
            ],
            when: "You need an overview of open work on the board.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
        Spec {
            name: "board_show",
            description: "Show one card (fields, reviews, questions, links, dependencies, recent events), \
                          or its packet with --packet.",
            input_schema: empty.clone(),
            cli_command: &["show"],
            selectors: &["<N|TASK-N>", "--packet"],
            when: "Before updating a card (you need its version) or to read a TASK-N you were pointed at.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
        Spec {
            name: "board_create",
            description: "Create a card in the board's first column; you are recorded as its creator.",
            input_schema: json!({
                "type": "object",
                "properties": create_props,
                "required": ["title"],
                "additionalProperties": false
            }),
            cli_command: &["create"],
            selectors: &[],
            when: "The user asked you to put work on the board.",
            input_summary: "{ title, description?, type?, priority?, size?, tier?, labels?, skills?, routing?, column?, metadata?, change_id? }",
            reads_stdin: true,
        },
        Spec {
            name: "board_update",
            description: "Update card fields. Pass the version you read; a stale version is board_version_conflict \
                          — show the card again and retry.",
            input_schema: json!({
                "type": "object",
                "properties": update_fields(),
                "required": ["version"],
                "additionalProperties": false
            }),
            cli_command: &["update"],
            selectors: &["<N|TASK-N>"],
            when: "A card you work on needs a corrected title, description or fields.",
            input_summary: "{ version, title?, description?, type?, priority?, size?, tier?, labels?, skills?, routing?, metadata?, change_id?, assignee_conversation? } (--assignee-conversation <id>)",
            reads_stdin: true,
        },
        Spec {
            name: "board_move",
            description: "Move a card to a column. Department agents cannot move cards (every studio column is \
                          closed to agents); the project's Board Keeper may move between non-gate columns. \
                          Gates follow the board's agent_access: `full` = agents move cards into, out of and \
                          past every gate; `approval` = only with --approval-note saying how the owner approved \
                          (recorded on the card). `deploying` is never entered by a move. \
                          Optional stdin places it before/after another card number.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "before": { "type": "integer", "description": "Card number to land before." },
                    "after": { "type": "integer", "description": "Card number to land after." },
                    "expected_column": { "type": "string", "description": "Refuse if the card left this column meanwhile." },
                    "approval_note": { "type": "string", "maxLength": 500, "description": "How the owner approved crossing a gate (boards with agent_access: approval); also --approval-note." }
                },
                "additionalProperties": false
            }),
            cli_command: &["move"],
            selectors: &["<N|TASK-N>", "<column>"],
            when: "You are the Board Keeper and the owner asked you to move a card.",
            input_summary: "{ before?, after?, expected_column?, approval_note? } (stdin optional; --approval-note)",
            reads_stdin: true,
        },
        Spec {
            name: "board_review",
            description: "Record your department's verdict on a card you were assigned to review (the department \
                          must be the one you were assigned). `questions` become open questions on the card.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "verdict": { "enum": ["pending", "approve", "change", "reject", "blocked"] },
                    "summary": { "type": "string", "maxLength": 4000 },
                    "findings": {
                        "type": "array",
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": { "enum": ["info", "low", "medium", "high", "critical"] },
                                "claim": { "type": "string", "maxLength": 500 },
                                "evidence": { "type": "string", "maxLength": 500 }
                            },
                            "required": ["severity", "claim"],
                            "additionalProperties": false
                        }
                    },
                    "recommendation": { "type": "string", "maxLength": 4000 },
                    "assumptions": string_array("At most 20 assumptions."),
                    "questions": {
                        "type": "array",
                        "maxItems": 10,
                        "items": {
                            "type": "object",
                            "properties": {
                                "to": { "type": "string", "description": "A department, or `owner`." },
                                "text": { "type": "string" }
                            },
                            "required": ["to", "text"],
                            "additionalProperties": false
                        }
                    },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                    "tokens_used": { "type": "integer", "minimum": 0 }
                },
                "required": ["verdict"],
                "additionalProperties": false
            }),
            cli_command: &["review"],
            selectors: &["<N|TASK-N>", "<department>"],
            when: "Your packet assigned you a card to review for a department.",
            input_summary: "{ verdict, summary?, findings?, recommendation?, assumptions?, questions?, confidence?, tokens_used? }",
            reads_stdin: true,
        },
        Spec {
            name: "board_comment",
            description: "Comment on a card. --answers <question id> closes that open question.",
            input_schema: json!({
                "type": "object",
                "properties": { "body": { "type": "string", "maxLength": 4000, "description": "Markdown." } },
                "required": ["body"],
                "additionalProperties": false
            }),
            cli_command: &["comment"],
            selectors: &["<N|TASK-N>", "--to <department>", "--answers <question id>"],
            when: "You have a finding, a status note or an answer that belongs on the card.",
            input_summary: "{ body }",
            reads_stdin: true,
        },
        Spec {
            name: "board_block",
            description: "Mark a card blocked with a reason (blocked cards cannot move forward).",
            input_schema: empty.clone(),
            cli_command: &["block"],
            selectors: &["<N|TASK-N>", "--reason <text>"],
            when: "Work on the card cannot continue until something outside it changes.",
            input_summary: "(no stdin; --reason is required)",
            reads_stdin: false,
        },
        Spec {
            name: "board_unblock",
            description: "Clear a card's manual block.",
            input_schema: empty.clone(),
            cli_command: &["unblock"],
            selectors: &["<N|TASK-N>"],
            when: "The reason a card was blocked is gone.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
        Spec {
            name: "board_link",
            description: "Link THIS conversation to a card (role other, build or qa).",
            input_schema: empty.clone(),
            cli_command: &["link"],
            selectors: &["<N|TASK-N>", "--role other|build|qa"],
            when: "You are working on the card and the owner should find this conversation from it.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
        Spec {
            name: "board_events",
            description: "Read this project's board history by seq (paged).",
            input_schema: empty,
            cli_command: &["events"],
            selectors: &["--after-seq <seq>", "--limit <n>"],
            when: "You need to know what changed on the board since a known point.",
            input_summary: "(no stdin)",
            reads_stdin: false,
        },
    ]
}

pub fn board_tool_descriptors() -> Vec<BoardToolDescriptor> {
    specs()
        .into_iter()
        .map(|spec| BoardToolDescriptor {
            name: spec.name.to_owned(),
            description: spec.description.to_owned(),
            input_schema: spec.input_schema,
            cli_command: spec.cli_command.iter().map(|part| (*part).to_owned()).collect(),
            selectors: spec.selectors.iter().map(|part| (*part).to_owned()).collect(),
            when: spec.when.to_owned(),
            input_summary: spec.input_summary.to_owned(),
            reads_stdin: spec.reads_stdin,
        })
        .collect()
}

/// The descriptor behind a CLI subcommand (`create`, `update`, …).
pub fn board_tool_for_cli_command(command: &str) -> Option<BoardToolDescriptor> {
    board_tool_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.cli_command.len() == 1 && descriptor.cli_command[0] == command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stdin_command_has_a_closed_object_schema() {
        for descriptor in board_tool_descriptors() {
            assert_eq!(descriptor.input_schema["type"], "object", "{}", descriptor.name);
            assert_eq!(
                descriptor.input_schema["additionalProperties"], false,
                "{}",
                descriptor.name
            );
            if !descriptor.reads_stdin {
                assert_eq!(descriptor.input_schema["properties"], json!({}), "{}", descriptor.name);
            }
        }
    }

    #[test]
    fn update_requires_the_version_and_create_the_title() {
        assert_eq!(
            board_tool_for_cli_command("update").unwrap().input_schema["required"],
            json!(["version"])
        );
        assert_eq!(
            board_tool_for_cli_command("create").unwrap().input_schema["required"],
            json!(["title"])
        );
        assert!(
            board_tool_for_cli_command("recover").is_none(),
            "recover is an operator command"
        );
    }
}
