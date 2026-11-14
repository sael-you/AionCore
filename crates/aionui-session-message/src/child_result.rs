//! `ChildResultDelivery` over the cross-session message path.
//!
//! A finished child conversation reports its final answer to its parent by
//! sending the parent a normal cross-session message FROM the child. Riding
//! `SessionMessageService::send` means the report honours the per-user
//! cross-session toggle (migration 041), the rate limiter, the busy-target
//! queue, and the team-conversation refusals exactly like an agent-sent
//! message — there is no second delivery path.

use std::sync::Arc;

use aionui_api_types::SessionSendMessageRequest;
use aionui_common::ChildResultDelivery;
use async_trait::async_trait;

use crate::service::SessionMessageService;

pub struct SessionChildResultDelivery {
    service: Arc<SessionMessageService>,
}

impl SessionChildResultDelivery {
    pub fn new(service: Arc<SessionMessageService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl ChildResultDelivery for SessionChildResultDelivery {
    async fn deliver_child_result(
        &self,
        user_id: &str,
        child_conversation_id: &str,
        parent_conversation_id: &str,
        message: String,
    ) -> Result<(), String> {
        // Reports only flow child -> parent; a self-addressed one would be a loop.
        if child_conversation_id == parent_conversation_id {
            return Err("child and parent are the same conversation".to_owned());
        }
        self.service
            .send(
                user_id,
                child_conversation_id,
                &SessionSendMessageRequest {
                    to: parent_conversation_id.to_owned(),
                    message,
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}
