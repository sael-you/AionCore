//! Draft text helpers: filler removal, the transcript→draft diff the card
//! shows transform by transform (§3.3.2), and the read-back template (§3.3.3).

use serde::Serialize;

use super::state::Delivery;

const FILLERS: &[&str] = &["um", "uh", "er", "erm", "uhm", "hmm", "like", "basically", "actually"];

/// Drop filler words, keep everything else verbatim.
pub fn strip_fillers(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| {
            let bare: String = w
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            !FILLERS.contains(&bare.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffOp {
    /// `equal` | `insert` | `delete`.
    pub op: &'static str,
    pub text: String,
    /// `filler` | `reference` | `reorder` for non-equal ops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<&'static str>,
}

fn bare(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// Word-level LCS diff from the transcript to the draft text. Deleted fillers
/// are tagged `filler`, inserted words that come from a resolved reference
/// are tagged `reference`, other changes `reorder` (headings, moved text).
pub fn diff(transcript: &str, draft: &str, references: &[String]) -> Vec<DiffOp> {
    let a: Vec<&str> = transcript.split_whitespace().collect();
    let b: Vec<&str> = draft.split_whitespace().collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if bare(a[i]) == bare(b[j]) && !bare(a[i]).is_empty() {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let reference_words: Vec<String> = references
        .iter()
        .flat_map(|r| {
            r.split(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|w| !w.is_empty())
        .collect();
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut push = |op: &'static str, word: &str, transform: Option<&'static str>| match ops.last_mut() {
        Some(last) if last.op == op && last.transform == transform => {
            last.text.push(' ');
            last.text.push_str(word);
        }
        _ => ops.push(DiffOp {
            op,
            text: word.to_owned(),
            transform,
        }),
    };
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && bare(a[i]) == bare(b[j]) && !bare(a[i]).is_empty() {
            push("equal", b[j], None);
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            let tag = if reference_words.contains(&bare(b[j])) {
                "reference"
            } else {
                "reorder"
            };
            push("insert", b[j], Some(tag));
            j += 1;
        } else {
            let tag = if FILLERS.contains(&bare(a[i]).as_str()) {
                "filler"
            } else {
                "reorder"
            };
            push("delete", a[i], Some(tag));
            i += 1;
        }
    }
    ops
}

/// Read-back text: operation + target name + short summary + delivery mode.
/// Never contains a confirm-grammar word (the caller re-checks).
pub fn readback_text(target_name: &str, summary: &str, delivery: Delivery) -> String {
    let delivery = match delivery {
        Delivery::NewTurn => "It starts a new turn.",
        Delivery::Steer => "It joins the turn that is running now.",
        Delivery::QueueBusy => "It waits until the current turn ends.",
        Delivery::QueueApproval => "It waits for an approval first.",
        Delivery::NewConversation => "It opens a new conversation.",
    };
    let summary: String = summary.split_whitespace().take(18).collect::<Vec<_>>().join(" ");
    format!("Message for {target_name}: {summary}. {delivery}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drafts::evidence::contains_confirm_grammar;

    #[test]
    fn fillers_are_removed() {
        assert_eq!(strip_fillers("um so uh rerun the, like, tests"), "so rerun the, tests");
    }

    #[test]
    fn diff_tags_each_transform() {
        let ops = diff(
            "um tell ion core to add logging",
            "Goal: add logging to AionCore",
            &["AionCore".into()],
        );
        assert!(
            ops.iter()
                .any(|o| o.op == "delete" && o.transform == Some("filler") && o.text == "um")
        );
        assert!(
            ops.iter()
                .any(|o| o.op == "insert" && o.transform == Some("reference") && o.text.contains("AionCore"))
        );
        assert!(ops.iter().any(|o| o.op == "equal" && o.text.contains("add logging")));
        let json = serde_json::to_value(&ops).unwrap();
        assert!(json[0].get("op").is_some());
    }

    #[test]
    fn readback_has_no_confirm_words() {
        for delivery in [
            Delivery::NewTurn,
            Delivery::Steer,
            Delivery::QueueBusy,
            Delivery::QueueApproval,
            Delivery::NewConversation,
        ] {
            let text = readback_text("Kanban build", "rerun the failing tests", delivery);
            assert!(text.starts_with("Message for Kanban build: rerun the failing tests."));
            assert!(!contains_confirm_grammar(&text), "{text}");
        }
    }
}
