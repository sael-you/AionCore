//! In-memory fakes for the S1b ports: conversations (records sends) and
//! speech (synth / name-check transcriber / refiner).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use aionui_voice::drafts::{SpeechFactory, SpeechKit};
use aionui_voice::{
    Clip, ClipVoice, ConversationInfo, ConversationPort, ConversationStatus, MessageHit, ProjectInfo, RawRefinement,
    RefineInput, RefinerPort, SendOutcome, SendProbe, SynthPort, TranscribePort,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub conversation_id: String,
    pub text: String,
    pub key: String,
}

#[derive(Default)]
pub struct ConversationState {
    pub conversations: Vec<ConversationInfo>,
    pub projects: Vec<ProjectInfo>,
    pub statuses: std::collections::HashMap<String, ConversationStatus>,
    pub replies: std::collections::HashMap<String, String>,
    /// Every send attempt (idempotency keys included).
    pub sends: Vec<SentMessage>,
    /// Next send outcome override (`Busy`, `Rejected`…); `None` → delivered.
    pub next_outcomes: std::collections::VecDeque<SendOutcome>,
    pub probe: SendProbe,
}

#[derive(Default)]
pub struct FakeConversations {
    pub state: Mutex<ConversationState>,
}

impl FakeConversations {
    pub fn add(&self, id: &str, name: &str, project: Option<&str>) {
        self.state.lock().unwrap().conversations.push(ConversationInfo {
            id: id.into(),
            name: name.into(),
            project_id: project.map(str::to_owned),
            updated_at: 1,
        });
    }

    pub fn add_project(&self, id: &str, name: &str, root: Option<&str>) {
        self.state.lock().unwrap().projects.push(ProjectInfo {
            id: id.into(),
            name: name.into(),
            root: root.map(str::to_owned),
        });
    }

    pub fn set_status(&self, id: &str, status: ConversationStatus) {
        self.state.lock().unwrap().statuses.insert(id.into(), status);
    }

    pub fn push_outcome(&self, outcome: SendOutcome) {
        self.state.lock().unwrap().next_outcomes.push_back(outcome);
    }

    pub fn set_probe(&self, probe: SendProbe) {
        self.state.lock().unwrap().probe = probe;
    }

    pub fn sends(&self) -> Vec<SentMessage> {
        self.state.lock().unwrap().sends.clone()
    }

    /// Distinct messages delivered (the conversation dedupes by key).
    pub fn delivered_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.sends().into_iter().map(|s| s.key).collect();
        keys.sort();
        keys.dedup();
        keys
    }
}

#[async_trait::async_trait]
impl ConversationPort for FakeConversations {
    async fn list_conversations(&self, _user_id: &str) -> Result<Vec<ConversationInfo>, String> {
        Ok(self.state.lock().unwrap().conversations.clone())
    }

    async fn list_projects(&self, _user_id: &str) -> Result<Vec<ProjectInfo>, String> {
        Ok(self.state.lock().unwrap().projects.clone())
    }

    async fn status(&self, _user_id: &str, conversation_id: &str) -> Result<Option<ConversationStatus>, String> {
        let state = self.state.lock().unwrap();
        if !state.conversations.iter().any(|c| c.id == conversation_id) {
            return Ok(None);
        }
        Ok(Some(state.statuses.get(conversation_id).cloned().unwrap_or_default()))
    }

    async fn last_reply(&self, _user_id: &str, conversation_id: &str) -> Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().replies.get(conversation_id).cloned())
    }

    async fn search_messages(&self, _user_id: &str, keyword: &str, _limit: usize) -> Result<Vec<MessageHit>, String> {
        let state = self.state.lock().unwrap();
        Ok(state
            .replies
            .iter()
            .filter(|(_, text)| text.contains(keyword))
            .map(|(id, text)| MessageHit {
                conversation_id: id.clone(),
                conversation_name: state
                    .conversations
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                snippet: text.clone(),
            })
            .collect())
    }

    async fn send_message(
        &self,
        _user_id: &str,
        conversation_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> SendOutcome {
        let mut state = self.state.lock().unwrap();
        state.sends.push(SentMessage {
            conversation_id: conversation_id.into(),
            text: text.into(),
            key: idempotency_key.into(),
        });
        state
            .next_outcomes
            .pop_front()
            .unwrap_or_else(|| SendOutcome::Delivered {
                msg_id: format!("msg-{idempotency_key}"),
                turn_id: format!("turn-{idempotency_key}"),
                midturn: false,
            })
    }

    async fn probe_send(&self, _user_id: &str, _conversation_id: &str, _msg_id: &str) -> Result<SendProbe, String> {
        Ok(self.state.lock().unwrap().probe.clone())
    }

    async fn create_conversation(
        &self,
        _user_id: &str,
        project_id: Option<&str>,
        title: &str,
        draft_key: &str,
    ) -> Result<String, String> {
        let id = format!("new-{draft_key}");
        let mut state = self.state.lock().unwrap();
        if !state.conversations.iter().any(|c| c.id == id) {
            state.conversations.push(ConversationInfo {
                id: id.clone(),
                name: title.into(),
                project_id: project_id.map(str::to_owned),
                updated_at: 2,
            });
        }
        Ok(id)
    }
}

/// Speech fakes. The synth "speaks" the text as bytes; the transcriber hears
/// it back (optionally mangling a name); the refiner returns a fixed result.
#[derive(Clone, Default)]
pub struct FakeSpeech {
    pub refinement: Arc<Mutex<Option<RawRefinement>>>,
    /// Replace (from, to) in what the transcriber hears (misheard names).
    pub mishear: Arc<Mutex<Option<(String, String)>>>,
    pub no_synth: Arc<Mutex<bool>>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl FakeSpeech {
    /// Everything synthesised, in order.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    pub fn set_refinement(&self, refinement: Option<RawRefinement>) {
        *self.refinement.lock().unwrap() = refinement;
    }

    pub fn set_mishear(&self, from: &str, to: &str) {
        *self.mishear.lock().unwrap() = Some((from.into(), to.into()));
    }
}

struct Synth(FakeSpeech);
struct Transcriber(FakeSpeech);
struct Refiner(FakeSpeech);

#[async_trait::async_trait]
impl SynthPort for Synth {
    async fn synthesize(&self, text: &str, _voice: ClipVoice) -> Result<Clip, String> {
        self.0.lines.lock().unwrap().push(text.to_owned());
        Ok(Clip {
            bytes: text.as_bytes().to_vec(),
            mime: "audio/mpeg",
        })
    }

    fn voice_id(&self, voice: ClipVoice) -> String {
        format!("{voice:?}")
    }
}

#[async_trait::async_trait]
impl TranscribePort for Transcriber {
    async fn transcribe(&self, clip: &Clip, _hints: &[String]) -> Result<String, String> {
        let mut heard = String::from_utf8_lossy(&clip.bytes).into_owned();
        if let Some((from, to)) = self.0.mishear.lock().unwrap().clone() {
            heard = heard.replace(from.as_str(), &to);
        }
        Ok(heard)
    }
}

#[async_trait::async_trait]
impl RefinerPort for Refiner {
    async fn refine(&self, _input: &RefineInput) -> Result<RawRefinement, String> {
        self.0
            .refinement
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no refinement configured".into())
    }
}

#[async_trait::async_trait]
impl SpeechFactory for FakeSpeech {
    async fn for_user(&self, _user_id: &str) -> SpeechKit {
        let synth: Option<Arc<dyn SynthPort>> =
            (!*self.no_synth.lock().unwrap()).then(|| Arc::new(Synth(self.clone())) as Arc<dyn SynthPort>);
        SpeechKit {
            synth,
            transcriber: Some(Arc::new(Transcriber(self.clone()))),
            refiner: Some(Arc::new(Refiner(self.clone()))),
        }
    }
}
