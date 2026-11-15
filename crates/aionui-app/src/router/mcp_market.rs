//! MCP market composition (card C15): the catalogue port over the extension
//! registry, the assistant-binding port over the assistant service, and the
//! session-only auth state for market writes.

use std::sync::Arc;

use aionui_api_types::{AssistantDefaultListRequest, AssistantDefaultsRequest, UpdateAssistantRequest};
use aionui_auth::{AuthIdentityMode, AuthState};
use aionui_extension::ExtensionRegistry;
use aionui_mcp::market::CATALOG_EXTENSION;
use aionui_mcp::{IMcpAssistantBinder, IMcpTemplateCatalog};

use crate::config::IdentityMode;
use crate::services::AppServices;

/// Templates = `contributes.mcpServers` of the `studio-mcp-catalog` extension
/// only (an arbitrary extension cannot pin launch commands).
pub(crate) struct ExtensionTemplateCatalog {
    pub registry: ExtensionRegistry,
}

#[async_trait::async_trait]
impl IMcpTemplateCatalog for ExtensionTemplateCatalog {
    async fn entries(&self, user_id: &str) -> Result<Vec<serde_json::Value>, String> {
        self.registry
            .get_mcp_servers_for_user(user_id)
            .await
            .into_iter()
            .filter(|s| s.extension_name == CATALOG_EXTENSION)
            .map(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
            .collect()
    }
}

/// Market ids are appended to an assistant's FIXED MCP list (other picks
/// kept, R1 #4, R8) through the assistant service, which publishes
/// `AssistantMcpBindingChanged` itself.
pub(crate) struct AssistantMcpBinder {
    pub assistant: Arc<aionui_assistant::AssistantService>,
}

impl AssistantMcpBinder {
    async fn rewrite(
        &self,
        user_id: &str,
        assistant_id: &str,
        edit: impl FnOnce(&mut Vec<String>),
    ) -> Result<(), String> {
        let detail = self
            .assistant
            .get_detail_for_user(user_id, assistant_id, None)
            .await
            .map_err(|e| e.to_string())?;
        let mut ids = detail.defaults.mcps.value;
        edit(&mut ids);
        let req = UpdateAssistantRequest {
            defaults: Some(AssistantDefaultsRequest {
                mcps: Some(AssistantDefaultListRequest {
                    mode: "fixed".to_owned(),
                    value: ids,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.assistant
            .update_for_user(user_id, assistant_id, req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl IMcpAssistantBinder for AssistantMcpBinder {
    async fn bind(&self, user_id: &str, assistant_id: &str, mcp_server_id: &str) -> Result<(), String> {
        self.rewrite(user_id, assistant_id, |ids| {
            if !ids.iter().any(|id| id == mcp_server_id) {
                ids.push(mcp_server_id.to_owned());
            }
        })
        .await
    }

    async fn unbind(&self, user_id: &str, assistant_id: &str, mcp_server_id: &str) -> Result<(), String> {
        self.rewrite(user_id, assistant_id, |ids| ids.retain(|id| id != mcp_server_id))
            .await
    }
}

/// Identity for market writes: a validated session (cookie or bearer JWT) in
/// every mode, never the runtime-token channel and never `--local`'s default
/// user (the voice `AuthState{UserSession, runtime_token_verifier: None}`
/// pattern, R1 #1).
pub(crate) fn market_write_auth_state(services: &AppServices) -> AuthState {
    AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        identity_mode: if services.identity_mode == IdentityMode::AionPro {
            AuthIdentityMode::AionPro
        } else {
            AuthIdentityMode::UserSession
        },
        runtime_token_verifier: None,
        // C11: the session cookie is read under the configured name
        // (`__Host-aionui-session` in webui over HTTPS).
        cookie_names: services.cookie_config.names,
    }
}
