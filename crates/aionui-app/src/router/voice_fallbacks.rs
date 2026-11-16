//! Voice ports onto other domains (TASK-37): the owner's messaging channel
//! (`aionui-channel`) for `say` / `ask` with no voice device, and the skill
//! library (`aionui-extension`) for the `cs-voice-concierge` instructions.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_channel::manager::ChannelManager;
use aionui_channel::types::{OutgoingMessageType, UnifiedOutgoingMessage};
use aionui_db::{IChannelRepository, ISkillRepository};
use aionui_extension::{SKILL_MANIFEST_FILE, SkillPaths};
use aionui_voice::{ChannelPort, SkillPort};

/// Sends to the owner's most recently active paired chat whose plugin runs.
pub(crate) struct VoiceChannelAdapter {
    pub manager: Arc<ChannelManager>,
    pub repo: Arc<dyn IChannelRepository>,
}

impl VoiceChannelAdapter {
    /// `(plugin id, chat id)` of the newest chat of an authorized user whose
    /// platform plugin is running (built-in plugin ids are the platform type).
    async fn target(&self, owner: &str) -> Result<Option<(String, String)>, String> {
        let users = self.repo.get_all_users(owner).await.map_err(|e| e.to_string())?;
        let sessions = self.repo.get_all_sessions(owner).await.map_err(|e| e.to_string())?;
        Ok(sessions
            .into_iter()
            .filter_map(|session| {
                let chat = session.chat_id.clone()?;
                let user = users.iter().find(|u| u.id == session.user_id)?;
                self.manager
                    .is_plugin_running(owner, &user.platform_type)
                    .then(|| (session.last_activity, user.platform_type.clone(), chat))
            })
            .max_by_key(|(at, _, _)| *at)
            .map(|(_, plugin, chat)| (plugin, chat)))
    }
}

#[async_trait::async_trait]
impl ChannelPort for VoiceChannelAdapter {
    async fn send_text(&self, user_id: &str, text: &str) -> Result<bool, String> {
        let Some((plugin, chat)) = self.target(user_id).await? else {
            return Ok(false);
        };
        let message = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.to_owned()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        self.manager
            .send_message(user_id, &plugin, &chat, message)
            .await
            .map(|_| true)
            .map_err(|e| e.to_string())
    }
}

/// Reads a skill of the user's library (custom skills override built-ins).
pub(crate) struct VoiceSkillAdapter {
    pub paths: Arc<SkillPaths>,
    pub repo: Arc<dyn ISkillRepository>,
}

#[async_trait::async_trait]
impl SkillPort for VoiceSkillAdapter {
    async fn skill_markdown(&self, user_id: &str, name: &str) -> Option<String> {
        let items =
            aionui_extension::list_available_skills_with_repo_for_user(&self.paths, self.repo.as_ref(), user_id)
                .await
                .map_err(|err| tracing::warn!(error = %err, "voice: skill library unavailable"))
                .ok()?;
        let item = items.into_iter().find(|item| item.name == name)?;
        // Built-ins list the manifest itself; custom skills may list their folder.
        let mut path = PathBuf::from(&item.location);
        if tokio::fs::metadata(&path).await.ok()?.is_dir() {
            path.push(SKILL_MANIFEST_FILE);
        }
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| tracing::warn!(skill = name, error = %err, "voice: skill not readable"))
            .ok()
    }
}
