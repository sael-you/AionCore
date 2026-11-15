//! Write-only market secrets (R4/R5), encrypted with the provider-key
//! AES-256-GCM helpers (`aionui_common::encrypt_string` / `decrypt_string`,
//! key = `derive_encryption_key(storage-encryption secret)`; owner decision 1).

use aionui_api_types::{McpMarketEnvKind, McpMarketSecretStatus};
use aionui_common::{decrypt_string, encrypt_string};
use tracing::info;

use super::error::{MarketError, invalid};
use super::rules::SECRET_VALUE_MAX;
use super::service::McpMarketService;
use super::stored::StoredInstance;

impl McpMarketService {
    /// Status of every secret slot of every instance (names, `set`, `updated_at`).
    pub async fn list_secret_status(&self, user_id: &str) -> Result<Vec<McpMarketSecretStatus>, MarketError> {
        let mut out = Vec::new();
        for inst in self.list_instances(user_id).await? {
            out.extend(inst.secrets);
        }
        Ok(out)
    }

    /// The ref name and `required` flag of secret `key`: declared by the
    /// instance's template snapshot, or by the catalogue's current version of
    /// that template (so a secret a refresh will need can be set first; the
    /// launcher only ever decrypts keys of the snapshot it runs).
    async fn secret_slot(
        &self,
        user_id: &str,
        inst: &StoredInstance,
        key: &str,
    ) -> Result<(String, bool), MarketError> {
        let declared = |t: &aionui_api_types::McpMarketTemplate| {
            t.env
                .get(key)
                .filter(|f| f.kind == McpMarketEnvKind::Secret)
                .map(|f| f.required)
        };
        if let Some(required) = declared(&inst.template) {
            let name = inst
                .secret_refs
                .get(key)
                .cloned()
                .unwrap_or_else(|| super::rules::secret_ref_name(&inst.row.name, key));
            return Ok((name, required));
        }
        let current = super::template::find_template(self.catalog.as_ref(), user_id, &inst.template.id)
            .await
            .ok();
        match current.as_ref().and_then(declared) {
            Some(required) => Ok((super::rules::secret_ref_name(&inst.row.name, key), required)),
            None => Err(MarketError::NotFound(format!(
                "secret key {key} is not declared by template {}",
                inst.template.id
            ))),
        }
    }

    /// Set or replace one secret. The value is encrypted before it reaches the
    /// repository and is never logged or returned.
    pub async fn set_secret(
        &self,
        user_id: &str,
        instance_id: &str,
        key: &str,
        value: &str,
    ) -> Result<McpMarketSecretStatus, MarketError> {
        let inst = self.load(user_id, instance_id).await?;
        let (name, required) = self.secret_slot(user_id, &inst, key).await?;
        if value.is_empty() {
            return Err(invalid("secret value must not be empty; use DELETE to clear it"));
        }
        if value.len() > SECRET_VALUE_MAX {
            return Err(invalid(format!("secret value is larger than {SECRET_VALUE_MAX} bytes")));
        }
        if value.contains('\0') {
            return Err(invalid("secret value must not contain NUL"));
        }
        let encrypted = encrypt_string(value, &self.encryption_key)
            .map_err(|_| MarketError::Internal("secret encryption failed".to_owned()))?;
        let row = self
            .repo
            .upsert_secret(user_id, &inst.row.id, &name, &encrypted)
            .await?;
        info!(instance = %inst.row.name, secret = %name, "mcp market: secret set");
        Ok(McpMarketSecretStatus {
            key: key.to_owned(),
            name: name.clone(),
            instance_id: inst.row.id.clone(),
            instance_name: inst.row.name.clone(),
            required,
            set: true,
            updated_at: Some(row.updated_at),
        })
    }

    /// Clear one secret. An active instance whose required secret is cleared
    /// becomes dormant (it could not start anyway).
    pub async fn clear_secret(
        &self,
        user_id: &str,
        instance_id: &str,
        key: &str,
    ) -> Result<McpMarketSecretStatus, MarketError> {
        let inst = self.load(user_id, instance_id).await?;
        let (name, required) = self.secret_slot(user_id, &inst, key).await?;
        let removed = self.repo.delete_secret(user_id, &inst.row.id, &name).await?;
        if removed && required && inst.status == "active" && inst.template.env.contains_key(key) {
            self.repo
                .update_instance(
                    user_id,
                    &inst.row.id,
                    &aionui_db::UpdateMarketInstanceParams {
                        status: Some("dormant"),
                        ..Default::default()
                    },
                )
                .await?;
        }
        info!(instance = %inst.row.name, secret = %name, removed, "mcp market: secret cleared");
        Ok(McpMarketSecretStatus {
            key: key.to_owned(),
            name: name.clone(),
            instance_id: inst.row.id.clone(),
            instance_name: inst.row.name.clone(),
            required,
            set: false,
            updated_at: None,
        })
    }

    /// Decrypt exactly the secrets the instance's template declares and the
    /// instance owns (R1 #1). Backend-internal: test-connection and the launcher.
    pub(crate) async fn decrypt_instance_secrets(
        &self,
        user_id: &str,
        inst: &StoredInstance,
    ) -> Result<Vec<(String, String)>, MarketError> {
        decrypt_declared(self.repo.as_ref(), &self.encryption_key, user_id, inst).await
    }
}

/// See [`McpMarketService::decrypt_instance_secrets`]; shared with the launcher.
pub(crate) async fn decrypt_declared(
    repo: &dyn aionui_db::IMcpMarketRepository,
    key: &[u8; 32],
    user_id: &str,
    inst: &StoredInstance,
) -> Result<Vec<(String, String)>, MarketError> {
    let rows = repo.list_instance_secrets(user_id, &inst.row.id).await?;
    let mut out = Vec::new();
    for env_key in inst.secret_keys() {
        let Some(name) = inst.secret_refs.get(&env_key) else {
            continue;
        };
        let Some(row) = rows.iter().find(|r| &r.name == name && r.instance_id == inst.row.id) else {
            continue;
        };
        let value = decrypt_string(&row.value_encrypted, key)
            .map_err(|_| MarketError::Internal(format!("secret {name} cannot be decrypted with the current key")))?;
        out.push((env_key, value));
    }
    Ok(out)
}
