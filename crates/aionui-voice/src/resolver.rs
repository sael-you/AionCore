//! Deterministic target resolver (§3.3.2) — "the real control" over which
//! conversation a draft goes to. It matches the transcription-model
//! transcript (never the conversational model's words) against the user's own
//! conversation names with normalised tokens and a phonetic key. A target is
//! chosen only when it is the unique top match (score ≥ τ, margin ≥ δ) or the
//! current focus; otherwise the draft is ambiguous and Jarvis asks "A or B?".
//! The refiner and tool results can never add an id outside this set.

/// Minimum score of a unique match.
pub const TAU: f64 = 0.75;
/// Minimum lead over the runner-up.
pub const DELTA: f64 = 0.2;
/// Candidates scoring at least this are offered in "A or B?".
const WEAK: f64 = 0.45;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    /// Name of the project the conversation belongs to (adds a small bonus
    /// when the transcript mentions it).
    pub project_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Unique top match.
    Unique { id: String, name: String, score: f64 },
    /// No name in the transcript; the conversation in focus.
    Focus { id: String },
    /// Several plausible targets: ask (at most 3 offered).
    Ambiguous { options: Vec<(String, String)> },
    /// Nothing matched and nothing is in focus.
    Unresolved,
}

/// Words that never identify a target.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "to",
    "in",
    "on",
    "for",
    "of",
    "and",
    "or",
    "my",
    "our",
    "your",
    "this",
    "that",
    "it",
    "is",
    "be",
    "please",
    "tell",
    "ask",
    "send",
    "message",
    "conversation",
    "chat",
    "session",
    "thread",
    "project",
    "jarvis",
    "hey",
    "ok",
    "okay",
    "so",
    "um",
    "uh",
    "er",
    "like",
    "just",
    "can",
    "you",
    "could",
    "would",
    "with",
    "about",
    "there",
    "here",
    "then",
    "now",
    "go",
    "run",
];

/// Lowercase alphanumeric tokens; camelCase and digit boundaries split.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut prev: Option<char> = None;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            let boundary = matches!(prev, Some(p) if (p.is_lowercase() && ch.is_uppercase())
                || (p.is_alphabetic() && ch.is_numeric())
                || (p.is_numeric() && ch.is_alphabetic()));
            if boundary && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        prev = Some(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn content_tokens(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Consonant skeleton: "aion"/"ion"/"eon" → "n", "clauterm"/"clawterm" → "kltrm".
pub fn phonetic(token: &str) -> String {
    let mut key = String::new();
    let chars: Vec<char> = token.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let mapped = match c {
            'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'w' | 'h' => None,
            'p' if next == Some('h') => {
                i += 1;
                Some('f')
            }
            'c' | 'k' | 'q' => Some('k'),
            'x' => Some('k'),
            'z' | 's' => Some('s'),
            'd' | 't' => Some('t'),
            'b' | 'p' => Some('p'),
            'g' | 'j' => Some('j'),
            'v' | 'f' => Some('f'),
            'm' | 'n' => Some(c),
            other => Some(other),
        };
        if let Some(m) = mapped
            && key.chars().last() != Some(m)
        {
            key.push(m);
        }
        i += 1;
    }
    key
}

/// How well one name token is heard in the transcript tokens.
fn token_match(name_token: &str, heard: &[String]) -> f64 {
    let key = phonetic(name_token);
    heard.iter().fold(0.0_f64, |best, h| {
        let score = if h == name_token {
            1.0
        } else if !key.is_empty() && key == phonetic(h) && name_token.chars().count() >= 3 {
            0.85
        } else if name_token.len() >= 5
            && h.len() >= 4
            && (name_token.starts_with(h.as_str()) || h.starts_with(name_token))
        {
            0.7
        } else {
            0.0
        };
        best.max(score)
    })
}

/// Score in `[0, 1.1]`: mean token match over the name's content tokens,
/// plus a small bonus when its project is mentioned too.
pub fn score(candidate: &Candidate, heard: &[String]) -> f64 {
    let name_tokens = content_tokens(&candidate.name);
    if name_tokens.is_empty() {
        return 0.0;
    }
    let total: f64 = name_tokens.iter().map(|t| token_match(t, heard)).sum();
    let mut score = total / name_tokens.len() as f64;
    if score > 0.0
        && let Some(project) = &candidate.project_name
    {
        let project_tokens = content_tokens(project);
        if !project_tokens.is_empty() && project_tokens.iter().all(|t| token_match(t, heard) >= 0.85) {
            score += 0.1;
        }
    }
    score
}

/// Resolve the target named in `transcript` among `candidates`.
pub fn resolve(transcript: &str, candidates: &[Candidate], focus: Option<&str>) -> Resolution {
    let heard = content_tokens(transcript);
    let mut scored: Vec<(f64, &Candidate)> = candidates
        .iter()
        .map(|c| (score(c, &heard), c))
        .filter(|(s, _)| *s >= WEAK)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    match scored.as_slice() {
        [] => match focus {
            Some(id) if candidates.iter().any(|c| c.id == id) => Resolution::Focus { id: id.to_owned() },
            _ => Resolution::Unresolved,
        },
        [(top, best), rest @ ..] => {
            let runner_up = rest.first().map_or(0.0, |(s, _)| *s);
            if *top >= TAU && top - runner_up >= DELTA {
                Resolution::Unique {
                    id: best.id.clone(),
                    name: best.name.clone(),
                    score: *top,
                }
            } else {
                Resolution::Ambiguous {
                    options: scored
                        .iter()
                        .take(3)
                        .map(|(_, c)| (c.id.clone(), c.name.clone()))
                        .collect(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, name: &str, project: Option<&str>) -> Candidate {
        Candidate {
            id: id.into(),
            name: name.into(),
            project_name: project.map(str::to_owned),
        }
    }

    fn catalogue() -> Vec<Candidate> {
        vec![
            c("c-spec", "Kanban spec", Some("aionui")),
            c("c-build", "Kanban build", Some("aionui")),
            c("c-core", "AionCore voice slice", Some("aionui")),
            c("c-studio", "clauterm-studio planning", Some("clauterm")),
            c("c-login", "Fix login bug", Some("aionui")),
        ]
    }

    #[test]
    fn tokens_split_camel_case_digits_and_punctuation() {
        assert_eq!(
            tokens("AionCore S1a-voice"),
            vec!["aion", "core", "s", "1", "a", "voice"]
        );
        assert_eq!(tokens("clauterm-studio"), vec!["clauterm", "studio"]);
    }

    #[test]
    fn phonetic_keys_tolerate_misheard_names() {
        assert_eq!(phonetic("aion"), phonetic("ion"));
        assert_eq!(phonetic("aion"), phonetic("eon"));
        assert_eq!(phonetic("clauterm"), phonetic("clawterm"));
        assert_ne!(phonetic("spec"), phonetic("build"));
    }

    #[test]
    fn homophones_pick_the_spoken_one() {
        let cat = catalogue();
        assert!(matches!(
            resolve("tell the kanban build to rerun the tests", &cat, None),
            Resolution::Unique { ref id, .. } if id == "c-build"
        ));
        assert!(matches!(
            resolve("add the edge cases to the kanban spec", &cat, None),
            Resolution::Unique { ref id, .. } if id == "c-spec"
        ));
    }

    #[test]
    fn shared_prefix_alone_is_ambiguous() {
        match resolve("tell kanban to hurry up", &catalogue(), None) {
            Resolution::Ambiguous { options } => {
                let ids: Vec<_> = options.iter().map(|(id, _)| id.as_str()).collect();
                assert!(ids.contains(&"c-spec") && ids.contains(&"c-build"), "{ids:?}");
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn misheard_names_still_resolve() {
        assert!(matches!(
            resolve("in the ion core voice slice, add logging", &catalogue(), None),
            Resolution::Unique { ref id, .. } if id == "c-core"
        ));
        assert!(matches!(
            resolve("clawterm studio planning needs the new plan", &catalogue(), None),
            Resolution::Unique { ref id, .. } if id == "c-studio"
        ));
    }

    #[test]
    fn focus_only_when_no_name_is_heard() {
        assert_eq!(
            resolve("add a retry around the upload", &catalogue(), Some("c-login")),
            Resolution::Focus { id: "c-login".into() }
        );
        // A spoken name beats the focus.
        assert!(matches!(
            resolve("tell the kanban build to stop", &catalogue(), Some("c-login")),
            Resolution::Unique { ref id, .. } if id == "c-build"
        ));
        // A focus that is not one of the user's conversations is ignored.
        assert_eq!(
            resolve("add a retry", &catalogue(), Some("someone-elses")),
            Resolution::Unresolved
        );
    }

    #[test]
    fn near_duplicates_are_ambiguous() {
        let cat = vec![c("a", "Voice plan review", None), c("b", "Voice plan reviews", None)];
        assert!(matches!(
            resolve("voice plan review", &cat, None),
            Resolution::Ambiguous { .. }
        ));
    }
}
