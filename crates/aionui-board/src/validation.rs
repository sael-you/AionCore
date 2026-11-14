//! Content limits (R9). Every violation is `422 board_invalid` with the
//! offending `field` in the details.

use serde_json::{Value, json};

use crate::error::{BoardError, BoardErrorCode, BoardResult};

pub const TITLE_MAX: usize = 200;
pub const DESCRIPTION_MAX: usize = 20_000;
pub const COMMENT_MAX: usize = 4_000;
pub const TAG_LIST_MAX: usize = 20;
pub const TAG_MAX: usize = 40;
pub const ROUTING_MAX: usize = 10;
pub const METADATA_MAX_BYTES: usize = 8 * 1024;
pub const RULE_MAX_BYTES: usize = 2 * 1024;
pub const COLUMN_NAME_MAX: usize = 60;
pub const COLUMNS_MAX: usize = 30;
pub const BLOCK_REASON_MAX: usize = 500;
pub const CHANGE_ID_MAX: usize = 200;
pub const DEPARTMENT_MAX: usize = 40;

fn too_long(field: &str, limit: usize) -> BoardError {
    BoardError::new(BoardErrorCode::Invalid, format!("{field} exceeds {limit} characters"))
        .with_details(json!({ "field": field, "limit": limit }))
}

/// A required single-line text: trimmed, non-empty, at most `max` chars.
pub fn required_text(field: &str, value: &str, max: usize) -> BoardResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(BoardError::invalid(field, format!("{field} must not be empty")));
    }
    if trimmed.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(trimmed.to_owned())
}

/// Free text (may be empty), at most `max` chars; kept verbatim.
pub fn free_text(field: &str, value: &str, max: usize) -> BoardResult<String> {
    if value.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(value.to_owned())
}

/// A list of short tags: each trimmed, non-empty, ≤ [`TAG_MAX`] chars;
/// duplicates dropped (first wins); at most `max_items` entries.
pub fn tag_list(field: &str, values: &[String], max_items: usize) -> BoardResult<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let tag = value.trim();
        if tag.is_empty() {
            return Err(BoardError::invalid(field, format!("{field} entries must not be empty")));
        }
        if tag.chars().count() > TAG_MAX {
            return Err(too_long(field, TAG_MAX));
        }
        if !out.iter().any(|existing| existing == tag) {
            out.push(tag.to_owned());
        }
    }
    if out.len() > max_items {
        return Err(BoardError::new(
            BoardErrorCode::Invalid,
            format!("{field} holds at most {max_items} entries"),
        )
        .with_details(json!({ "field": field, "limit": max_items })));
    }
    Ok(out)
}

/// A JSON object of at most `max_bytes` serialized bytes.
pub fn json_object(field: &str, value: &Value, max_bytes: usize) -> BoardResult<Value> {
    if !value.is_object() {
        return Err(BoardError::invalid(field, format!("{field} must be a JSON object")));
    }
    let size = serde_json::to_string(value)?.len();
    if size > max_bytes {
        return Err(
            BoardError::new(BoardErrorCode::Invalid, format!("{field} exceeds {max_bytes} bytes"))
                .with_details(json!({ "field": field, "limit": max_bytes })),
        );
    }
    Ok(value.clone())
}

/// Column keys: `^[a-z][a-z0-9_]{0,39}$` (they double as i18n keys).
pub fn column_key(value: &str) -> BoardResult<String> {
    let mut chars = value.chars();
    let valid = matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        && value.len() <= 40;
    if valid {
        Ok(value.to_owned())
    } else {
        Err(BoardError::invalid(
            "columns.key",
            format!("column key `{value}` must match ^[a-z][a-z0-9_]{{0,39}}$"),
        ))
    }
}

/// Parse a card reference from the agent router: `12` or `TASK-12`.
pub fn card_number(value: &str) -> BoardResult<i64> {
    let trimmed = value.trim();
    let digits = trimmed
        .strip_prefix("TASK-")
        .or_else(|| trimmed.strip_prefix("task-"))
        .unwrap_or(trimmed);
    digits
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| BoardError::invalid("number", format!("`{value}` is not a card number (N or TASK-N)")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_trimmed_required_and_bounded() {
        assert_eq!(required_text("title", "  Fix it ", TITLE_MAX).unwrap(), "Fix it");
        assert!(required_text("title", "   ", TITLE_MAX).is_err());
        let long = "x".repeat(TITLE_MAX + 1);
        let error = required_text("title", &long, TITLE_MAX).unwrap_err();
        assert_eq!(error.code(), Some(BoardErrorCode::Invalid));
        assert_eq!(error.details().unwrap()["field"], "title");
        assert!(
            required_text("title", &"é".repeat(TITLE_MAX), TITLE_MAX).is_ok(),
            "chars, not bytes"
        );
    }

    #[test]
    fn tags_are_deduplicated_and_bounded() {
        let tags = vec!["a".to_owned(), " a ".to_owned(), "b".to_owned()];
        assert_eq!(tag_list("labels", &tags, TAG_LIST_MAX).unwrap(), ["a", "b"]);
        let many: Vec<String> = (0..21).map(|i| format!("t{i}")).collect();
        assert!(tag_list("labels", &many, TAG_LIST_MAX).is_err());
        assert!(tag_list("labels", &["x".repeat(41)], TAG_LIST_MAX).is_err());
        assert!(tag_list("labels", &[" ".to_owned()], TAG_LIST_MAX).is_err());
    }

    #[test]
    fn metadata_must_be_a_small_object() {
        assert!(json_object("metadata", &json!([1]), METADATA_MAX_BYTES).is_err());
        let big = json!({ "x": "y".repeat(METADATA_MAX_BYTES) });
        assert!(json_object("metadata", &big, METADATA_MAX_BYTES).is_err());
        assert!(json_object("metadata", &json!({ "a": 1 }), METADATA_MAX_BYTES).is_ok());
    }

    #[test]
    fn column_keys_and_card_numbers_parse_strictly() {
        assert!(column_key("in_progress").is_ok());
        assert!(column_key("In Progress").is_err());
        assert!(column_key("1st").is_err());
        assert_eq!(card_number("TASK-7").unwrap(), 7);
        assert_eq!(card_number("7").unwrap(), 7);
        assert!(card_number("0").is_err());
        assert!(card_number("TASK-x").is_err());
    }
}
