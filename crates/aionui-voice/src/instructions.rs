//! Session instructions. The real text is compiled from the
//! `cs-voice-concierge` skill in S1b (§3.7); S1a ships a short fixed prompt so
//! the call lifecycle can run. The hash is stored per session.

use sha2::{Digest, Sha256};

/// S1a placeholder instructions (no tools yet: the model only talks).
pub const BASE_INSTRUCTIONS: &str = "You are Jarvis, a concise voice concierge for the user's coding \
projects. Speak in one or two short sentences, without markdown. You cannot send messages or change \
anything yet; if asked to, say that this is not available yet.";

/// SHA-256 hex of the instructions (and tool schemas) sent to the provider.
pub fn instructions_hash(instructions: &str, tools: &[serde_json::Value]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(instructions.as_bytes());
    for tool in tools {
        hasher.update(b"\0");
        hasher.update(tool.to_string().as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Rough token estimate of the cached prefix (≈ 4 characters per token),
/// used for the first reservation before any usage is reported.
pub fn estimate_prefix_tokens(instructions: &str, tools: &[serde_json::Value]) -> u64 {
    let chars = instructions.len() + tools.iter().map(|t| t.to_string().len()).sum::<usize>();
    (chars as u64).div_ceil(4) + 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_changes_with_tools() {
        let a = instructions_hash(BASE_INSTRUCTIONS, &[]);
        let b = instructions_hash(BASE_INSTRUCTIONS, &[serde_json::json!({"name": "x"})]);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(estimate_prefix_tokens(BASE_INSTRUCTIONS, &[]) > 64);
    }
}
