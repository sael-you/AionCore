//! MiniMax chat completions (OpenAI-compatible `POST {host}/v1/chat/completions`)
//! for the `minimax-cascade` brain. Wire facts from S0-B (`design/voice/s0/minimax.md`
//! #2–#6, docs `api-reference/text-openai-api.md`): streamed `delta.content` and
//! `delta.tool_calls` fragments (by `index`), `stream_options.include_usage`,
//! `thinking: {type: "disabled"}` on `MiniMax-M3`, `service_tier: "priority"`,
//! `max_completion_tokens`, `reasoning_split: true` keeps any thinking out of
//! `content`. There is no `tool_choice`: the backend owns confirmations.

use serde_json::{Value, json};

use crate::pricing::Usage;

/// Default brain model (S0-B: thinking off, priority tier, 94/95 tool calls).
pub const MINIMAX_BRAIN_MODEL: &str = "MiniMax-M3";

/// A clause shorter than this waits for the next one (no tiny clips).
const MIN_SENTENCE_CHARS: usize = 12;
/// A clause with no punctuation is cut at a space after this many characters.
const MAX_SENTENCE_CHARS: usize = 220;

/// Realtime function tools (`{type, name, description, parameters}`) in the
/// chat-completions shape (`{type, function: {name, description, parameters}}`).
pub(crate) fn chat_tools(realtime_tools: &[Value]) -> Vec<Value> {
    realtime_tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?;
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                }
            }))
        })
        .collect()
}

/// The streamed request of one brain turn.
pub(crate) fn request_body(
    model: &str,
    instructions: &str,
    history: &[Value],
    tools: &[Value],
    max_output_tokens: u64,
) -> Value {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(json!({"role": "system", "content": instructions}));
    messages.extend(history.iter().cloned());
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "thinking": {"type": "disabled"},
        "reasoning_split": true,
        "service_tier": "priority",
        "max_completion_tokens": max_output_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    body
}

/// One tool call assembled from its streamed fragments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ToolCallAcc {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What one SSE `data:` line contributed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ChunkDelta {
    pub content: String,
    pub done: bool,
    pub error: Option<String>,
}

/// Accumulates a streamed completion.
#[derive(Debug, Default)]
pub(crate) struct ChatStream {
    pub text: String,
    pub tool_calls: Vec<ToolCallAcc>,
    pub usage: Option<Usage>,
    line: String,
}

impl ChatStream {
    /// Feed raw response bytes; returns the content deltas of every complete line.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<ChunkDelta> {
        self.line.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.line.find('\n') {
            let line: String = self.line.drain(..=pos).collect();
            if let Some(delta) = self.line_delta(line.trim()) {
                out.push(delta);
            }
        }
        out
    }

    /// Flush a final unterminated line.
    pub(crate) fn finish(&mut self) -> Option<ChunkDelta> {
        let line = std::mem::take(&mut self.line);
        self.line_delta(line.trim())
    }

    fn line_delta(&mut self, line: &str) -> Option<ChunkDelta> {
        let data = line.strip_prefix("data:")?.trim();
        if data == "[DONE]" {
            return Some(ChunkDelta {
                done: true,
                ..Default::default()
            });
        }
        let chunk: Value = serde_json::from_str(data).ok()?;
        let mut delta = ChunkDelta::default();
        if let Some(code) = chunk["base_resp"]["status_code"].as_i64()
            && code != 0
        {
            delta.error = Some(format!("minimax status {code}"));
        }
        if let Some(message) = chunk["error"]["message"].as_str() {
            delta.error = Some(message.chars().take(120).collect());
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(map_usage(usage));
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let d = &choice["delta"];
            if let Some(text) = d["content"].as_str() {
                delta.content.push_str(text);
            }
            for call in d["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                if self.tool_calls.len() <= index {
                    self.tool_calls.resize_with(index + 1, ToolCallAcc::default);
                }
                let acc = &mut self.tool_calls[index];
                if let Some(id) = call["id"].as_str() {
                    acc.id = id.to_owned();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    acc.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    acc.arguments.push_str(args);
                }
            }
        }
        self.text.push_str(&delta.content);
        Some(delta)
    }

    /// Complete tool calls (named), with a stable id when the provider sent none.
    pub(crate) fn finished_tool_calls(&self, response_id: &str) -> Vec<ToolCallAcc> {
        self.tool_calls
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| ToolCallAcc {
                id: if c.id.is_empty() {
                    format!("{response_id}_call_{i}")
                } else {
                    c.id.clone()
                },
                name: c.name.clone(),
                arguments: if c.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    c.arguments.clone()
                },
            })
            .collect()
    }
}

/// Chat usage → engine usage (text only; cached prompt tokens when reported).
pub(crate) fn map_usage(usage: &Value) -> Usage {
    let input = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let output = usage["completion_tokens"].as_u64().unwrap_or(0);
    let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        input_text: input,
        cached_text: cached.min(input),
        output_text: output,
        ..Default::default()
    }
}

/// Cuts streamed text into speakable sentences: TTS starts on the first one
/// while the model is still writing the rest (S0-B "first speakable clause").
#[derive(Debug, Default)]
pub(crate) struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    pub(crate) fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(cut) = self.next_cut() {
            let sentence: String = self.buf.drain(..cut).collect();
            let sentence = sentence.trim();
            if !sentence.is_empty() {
                out.push(sentence.to_owned());
            }
        }
        out
    }

    /// The rest, at the end of the stream.
    pub(crate) fn flush(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        let rest = rest.trim();
        (!rest.is_empty()).then(|| rest.to_owned())
    }

    /// Byte index after a sentence end followed by whitespace, once the
    /// sentence is long enough; or a forced cut of an overlong clause.
    fn next_cut(&self) -> Option<usize> {
        let chars: Vec<(usize, char)> = self.buf.char_indices().collect();
        for (n, (i, c)) in chars.iter().enumerate() {
            let boundary = matches!(c, '.' | '!' | '?' | '…' | '\n')
                && chars.get(n + 1).is_some_and(|(_, next)| next.is_whitespace());
            if boundary && self.buf[..*i].trim().chars().count() + 1 >= MIN_SENTENCE_CHARS {
                return Some(i + c.len_utf8());
            }
        }
        if chars.len() > MAX_SENTENCE_CHARS {
            let limit = chars[MAX_SENTENCE_CHARS].0;
            return self.buf[..limit].rfind(' ').filter(|&p| p > 0).or(Some(limit));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_convert_to_the_chat_shape() {
        let realtime = vec![json!({"type": "function", "name": "list_projects", "description": "d",
            "parameters": {"type": "object", "properties": {}}})];
        let chat = chat_tools(&realtime);
        assert_eq!(chat[0]["type"], "function");
        assert_eq!(chat[0]["function"]["name"], "list_projects");
        assert_eq!(chat[0]["function"]["parameters"]["type"], "object");
        assert!(chat[0].get("name").is_none());
    }

    #[test]
    fn request_turns_thinking_off_on_the_priority_tier() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let body = request_body("MiniMax-M3", "be brief", &history, &[], 600);
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_completion_tokens"], 600);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert!(body.get("tools").is_none(), "no empty tools array");
    }

    #[test]
    fn stream_assembles_text_tool_calls_and_usage_across_split_chunks() {
        let mut s = ChatStream::default();
        let a = r#"data: {"choices":[{"delta":{"content":"Checking"}}]}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"list_","arguments":"{\"pro"}}]}}]}
da"#;
        let b = r#"ta: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"projects","arguments":"\"}"}}]}}]}
data: {"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":100}}}
data: [DONE]
"#;
        let first = s.feed(a.as_bytes());
        assert_eq!(first[0].content, "Checking");
        let second = s.feed(b.as_bytes());
        assert!(second.last().unwrap().done);
        assert_eq!(s.text, "Checking");
        let calls = s.finished_tool_calls("r1");
        assert_eq!(
            calls,
            vec![ToolCallAcc {
                id: "call_9".into(),
                name: "list_projects".into(),
                arguments: "{\"pro\"}".into()
            }]
        );
        let usage = s.usage.unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.cached_text),
            (120, 9, 100)
        );
    }

    #[test]
    fn stream_reports_provider_errors() {
        let mut s = ChatStream::default();
        let out = s.feed(b"data: {\"base_resp\":{\"status_code\":1008,\"status_msg\":\"insufficient balance\"}}\n");
        assert_eq!(out[0].error.as_deref(), Some("minimax status 1008"));
    }

    #[test]
    fn splitter_emits_sentences_as_they_complete() {
        let mut s = SentenceSplitter::default();
        assert!(s.push("You have three projects").is_empty());
        assert_eq!(
            s.push(". The first is aionui. Then"),
            vec!["You have three projects.", "The first is aionui."]
        );
        // A sentence end waits for the whitespace after it ("3.5", "e.g").
        assert!(s.push(" clauterm.").is_empty());
        assert_eq!(s.flush().as_deref(), Some("Then clauterm."));
    }

    #[test]
    fn splitter_keeps_short_fragments_with_the_next_sentence() {
        let mut s = SentenceSplitter::default();
        // "Ok." is too short for its own clip.
        assert_eq!(s.push("Ok. It is running now. "), vec!["Ok. It is running now."]);
        assert_eq!(
            s.push("3.5 seconds"),
            Vec::<String>::new(),
            "a decimal point is no boundary"
        );
    }

    #[test]
    fn splitter_cuts_overlong_clauses_at_a_space() {
        let mut s = SentenceSplitter::default();
        let long = "word ".repeat(60);
        let out = s.push(&long);
        assert_eq!(out.len(), 1);
        assert!(out[0].chars().count() <= MAX_SENTENCE_CHARS);
    }
}
