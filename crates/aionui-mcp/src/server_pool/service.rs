//! Owner CRUD of the server pool (C36 card 1): validation, the write-only
//! private key (AES-256-GCM, the provider-key crypto, like `mcp_secrets`),
//! host-key normalisation and the audit list.

use std::sync::Arc;

use aionui_api_types::{
    CreateServerRequest, ListServerAuditQuery, SERVER_TIER_NOTE, ServerAuditResponse, ServerResponse,
    UpdateServerRequest,
};
use aionui_common::{decrypt_string, encrypt_string};
use aionui_db::models::{ServerAuditRow, ServerRow};
use aionui_db::{CreateServerParams, DbError, IServerPoolRepository, SERVER_NAME_TAKEN_MSG, UpdateServerParams};
use tracing::info;

use super::gate::asks_for_crucial;

pub const ENVIRONMENTS: &[&str] = &["production", "staging", "sandbox", "local"];
pub const TIERS: &[&str] = &["read", "write"];
const NAME_MAX: usize = 63;
const HOST_MAX: usize = 253;
const USER_MAX: usize = 32;
const KEY_MAX: usize = 16 * 1024;
const HOST_KEY_MAX: usize = 8 * 1024;
const NOTES_MAX: usize = 2000;
const LABEL_MAX: usize = 40;
const LABELS_MAX: usize = 20;
const HOST_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// Errors name fields and rules, never a key.
#[derive(Debug, thiserror::Error)]
pub enum ServerPoolError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NameTaken(String),
    #[error("{0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<DbError> for ServerPoolError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(msg) => Self::NotFound(msg),
            DbError::Conflict(msg) if msg == SERVER_NAME_TAKEN_MSG => Self::NameTaken(msg),
            DbError::Conflict(msg) => Self::Invalid(msg),
            other => {
                tracing::error!(error = %other, "server pool: database error");
                Self::Internal("database error".to_owned())
            }
        }
    }
}

fn invalid(msg: impl Into<String>) -> ServerPoolError {
    ServerPoolError::Invalid(msg.into())
}

#[derive(Clone)]
pub struct ServerPoolService {
    repo: Arc<dyn IServerPoolRepository>,
    encryption_key: [u8; 32],
}

impl ServerPoolService {
    pub fn new(repo: Arc<dyn IServerPoolRepository>, encryption_key: [u8; 32]) -> Self {
        Self { repo, encryption_key }
    }

    pub fn repo(&self) -> &Arc<dyn IServerPoolRepository> {
        &self.repo
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<ServerResponse>, ServerPoolError> {
        Ok(self
            .repo
            .list_servers(user_id)
            .await?
            .iter()
            .map(server_response)
            .collect())
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<ServerResponse, ServerPoolError> {
        self.repo
            .find_server(user_id, id)
            .await?
            .map(|row| server_response(&row))
            .ok_or_else(|| ServerPoolError::NotFound(format!("server {id} not found")))
    }

    pub async fn create(&self, user_id: &str, req: CreateServerRequest) -> Result<ServerResponse, ServerPoolError> {
        let name = clean_name(&req.name)?;
        let host = clean_host(&req.host)?;
        let port = clean_port(req.port.unwrap_or(22))?;
        let ssh_user = clean_user(&req.ssh_user)?;
        let key = clean_private_key(&req.private_key)?;
        let host_key = clean_host_key(req.host_key.as_deref().unwrap_or(""))?;
        let labels = clean_labels(req.labels.as_deref().unwrap_or(&[]))?;
        let environment = clean_enum(
            req.environment.as_deref().unwrap_or("staging"),
            ENVIRONMENTS,
            "environment",
        )?;
        let tier = clean_enum(req.tier.as_deref().unwrap_or("read"), TIERS, "tier")?;
        let notes = clean_notes(req.notes.as_deref().unwrap_or(""))?;
        let key_encrypted = self.encrypt(&key)?;
        let row = self
            .repo
            .create_server(&CreateServerParams {
                user_id,
                name: &name,
                host: &host,
                port: i64::from(port),
                ssh_user: &ssh_user,
                host_key: &host_key,
                labels: &labels,
                environment: &environment,
                tier: &tier,
                notes: &notes,
                enabled: req.enabled.unwrap_or(true),
                key_encrypted: Some(&key_encrypted),
            })
            .await?;
        info!(server = %row.name, environment = %row.environment, tier = %row.tier, "servers: created");
        Ok(server_response(&row))
    }

    pub async fn update(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateServerRequest,
    ) -> Result<ServerResponse, ServerPoolError> {
        let current = self
            .repo
            .find_server(user_id, id)
            .await?
            .ok_or_else(|| ServerPoolError::NotFound(format!("server {id} not found")))?;
        let name = req.name.as_deref().map(clean_name).transpose()?;
        let host = req.host.as_deref().map(clean_host).transpose()?;
        let port = req.port.map(clean_port).transpose()?.map(i64::from);
        let ssh_user = req.ssh_user.as_deref().map(clean_user).transpose()?;
        let labels = req.labels.as_deref().map(clean_labels).transpose()?;
        let environment = req
            .environment
            .as_deref()
            .map(|v| clean_enum(v, ENVIRONMENTS, "environment"))
            .transpose()?;
        let tier = req.tier.as_deref().map(|v| clean_enum(v, TIERS, "tier")).transpose()?;
        let notes = req.notes.as_deref().map(clean_notes).transpose()?;
        let key = req.private_key.as_deref().map(clean_private_key).transpose()?;
        // A new destination never inherits the old host's key (PLATFORM-CONTRACT §3).
        let destination_changed = host.as_deref().is_some_and(|h| h != current.host)
            || port.is_some_and(|p| p != current.port)
            || ssh_user.as_deref().is_some_and(|u| u != current.ssh_user);
        let host_key = match req.host_key.as_deref() {
            Some(v) => Some(clean_host_key(v)?),
            None if destination_changed => Some(String::new()),
            None => None,
        };
        if let Some(key) = key {
            let encrypted = self.encrypt(&key)?;
            self.repo.set_server_key(user_id, id, &encrypted).await?;
        }
        let row = self
            .repo
            .update_server(
                user_id,
                id,
                &UpdateServerParams {
                    name: name.as_deref(),
                    host: host.as_deref(),
                    port,
                    ssh_user: ssh_user.as_deref(),
                    host_key: host_key.as_deref(),
                    labels: labels.as_deref(),
                    environment: environment.as_deref(),
                    tier: tier.as_deref(),
                    notes: notes.as_deref(),
                    enabled: req.enabled,
                },
            )
            .await?;
        info!(server = %row.name, enabled = row.enabled, "servers: updated");
        Ok(server_response(&row))
    }

    pub async fn delete(&self, user_id: &str, id: &str) -> Result<(), ServerPoolError> {
        if !self.repo.delete_server(user_id, id).await? {
            return Err(ServerPoolError::NotFound(format!("server {id} not found")));
        }
        info!(server_id = %id, "servers: deleted");
        Ok(())
    }

    pub async fn audit(
        &self,
        user_id: &str,
        query: ListServerAuditQuery,
    ) -> Result<Vec<ServerAuditResponse>, ServerPoolError> {
        let limit = query.limit.unwrap_or(100).clamp(1, 500);
        let server_id = query.server_id.as_deref().filter(|s| !s.is_empty());
        Ok(self
            .repo
            .list_server_audit(user_id, server_id, limit)
            .await?
            .into_iter()
            .map(audit_response)
            .collect())
    }

    /// The decrypted private key of a server (helper use only, never an API).
    pub async fn private_key(&self, user_id: &str, id: &str) -> Result<Option<String>, ServerPoolError> {
        let Some(encrypted) = self.repo.server_key(user_id, id).await? else {
            return Ok(None);
        };
        decrypt_string(&encrypted, &self.encryption_key)
            .map(Some)
            .map_err(|_| ServerPoolError::Internal("cannot decrypt the server key".to_owned()))
    }

    fn encrypt(&self, key: &str) -> Result<String, ServerPoolError> {
        encrypt_string(key, &self.encryption_key)
            .map_err(|_| ServerPoolError::Internal("cannot encrypt the server key".to_owned()))
    }
}

/// The live servers whose grant decision for `conversation_id` is "included"
/// right now (C36 §3: re-resolved on every call, so a detach or a level
/// `remove` applies at the next tool call). Switched-off servers are kept:
/// the gate refuses them with a reason.
pub async fn granted_servers(
    kits: &dyn aionui_db::IKitRepository,
    pool: &dyn IServerPoolRepository,
    user_id: &str,
    conversation_id: &str,
) -> Result<Vec<ServerRow>, DbError> {
    let Some(scope) = kits.conversation_scope(user_id, conversation_id).await? else {
        return Ok(Vec::new());
    };
    let layers = kits.load_layers(user_id, &scope).await?;
    let resolution = aionui_db::kit_resolve::resolve(&scope, &layers);
    Ok(pool
        .list_servers(user_id)
        .await?
        .into_iter()
        .filter(|s| resolution.server_granted(&s.id))
        .collect())
}

pub fn server_response(row: &ServerRow) -> ServerResponse {
    ServerResponse {
        id: row.id.clone(),
        name: row.name.clone(),
        host: row.host.clone(),
        port: u16::try_from(row.port).unwrap_or(22),
        ssh_user: row.ssh_user.clone(),
        host_key: row.host_key.clone(),
        labels: serde_json::from_str(&row.labels).unwrap_or_default(),
        environment: row.environment.clone(),
        tier: row.tier.clone(),
        notes: row.notes.clone(),
        enabled: row.enabled,
        has_key: row.has_key,
        crucial_commands_ask: asks_for_crucial(row),
        tier_note: SERVER_TIER_NOTE.to_owned(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn audit_response(row: ServerAuditRow) -> ServerAuditResponse {
    ServerAuditResponse {
        id: row.id,
        conversation_id: row.conversation_id,
        server_id: row.server_id,
        server_name: row.server_name,
        channel: row.channel,
        decision: row.decision,
        reason: row.reason,
        command: row.command,
        exit_code: row.exit_code,
        duration_ms: row.duration_ms,
        created_at: row.created_at,
    }
}

// -- validation ----------------------------------------------------------------

pub(crate) fn clean_name(value: &str) -> Result<String, ServerPoolError> {
    let v = value.trim();
    let ok = !v.is_empty()
        && v.len() <= NAME_MAX
        && v.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(invalid(
            "name: 1–63 characters, lowercase letters, digits and '-', starting with a letter or digit",
        ));
    }
    Ok(v.to_owned())
}

/// No leading '-' (would become an ssh option), no spaces or shell syntax.
fn clean_host(value: &str) -> Result<String, ServerPoolError> {
    let v = value.trim();
    let ok = !v.is_empty()
        && v.len() <= HOST_MAX
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    if !ok {
        return Err(invalid("host: a DNS name or an IP address"));
    }
    Ok(v.to_owned())
}

fn clean_port(port: u16) -> Result<u16, ServerPoolError> {
    if port == 0 {
        return Err(invalid("port: 1–65535"));
    }
    Ok(port)
}

fn clean_user(value: &str) -> Result<String, ServerPoolError> {
    let v = value.trim();
    let ok = !v.is_empty()
        && v.len() <= USER_MAX
        && v.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok {
        return Err(invalid("ssh_user: a Unix user name"));
    }
    Ok(v.to_owned())
}

/// Stored keys only, unencrypted (a passphrase cannot be typed by an agent).
fn clean_private_key(value: &str) -> Result<String, ServerPoolError> {
    let v = value.trim().replace("\r\n", "\n");
    if v.is_empty() || v.len() > KEY_MAX {
        return Err(invalid("private_key: required, at most 16 KB"));
    }
    let header_ok = v.starts_with("-----BEGIN ") && v.lines().next().is_some_and(|l| l.ends_with("PRIVATE KEY-----"));
    if !header_ok || !v.contains("-----END ") {
        return Err(invalid(
            "private_key: an OpenSSH or PEM private key (-----BEGIN … PRIVATE KEY-----)",
        ));
    }
    if v.contains("ENCRYPTED") || v.contains("Proc-Type: 4,ENCRYPTED") {
        return Err(invalid("private_key: passphrase-protected keys are not supported"));
    }
    Ok(format!("{v}\n"))
}

/// Lines of `<type> <base64>` (an optional leading host field and a trailing
/// comment are dropped); empty = not set yet.
pub(crate) fn clean_host_key(value: &str) -> Result<String, ServerPoolError> {
    if value.len() > HOST_KEY_MAX {
        return Err(invalid("host_key: at most 8 KB"));
    }
    let mut lines = Vec::new();
    for raw in value
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let fields: Vec<&str> = raw.split_whitespace().collect();
        let start = fields.iter().position(|f| HOST_KEY_TYPES.contains(f));
        let Some(start) = start else {
            return Err(invalid(
                "host_key: each line must be '<key type> <base64 key>' (known_hosts format)",
            ));
        };
        let Some(key) = fields.get(start + 1) else {
            return Err(invalid("host_key: missing the base64 key"));
        };
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        {
            return Err(invalid("host_key: the key part must be base64"));
        }
        let line = format!("{} {}", fields[start], key);
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    Ok(lines.join("\n"))
}

fn clean_labels(values: &[String]) -> Result<String, ServerPoolError> {
    let mut out: Vec<String> = Vec::new();
    for v in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        if v.chars().count() > LABEL_MAX || v.chars().any(char::is_control) {
            return Err(invalid("labels: each at most 40 characters"));
        }
        if !out.iter().any(|o| o == v) {
            out.push(v.to_owned());
        }
    }
    if out.len() > LABELS_MAX {
        return Err(invalid("labels: at most 20"));
    }
    serde_json::to_string(&out).map_err(|e| ServerPoolError::Internal(e.to_string()))
}

fn clean_enum(value: &str, allowed: &[&str], field: &str) -> Result<String, ServerPoolError> {
    let v = value.trim();
    if !allowed.contains(&v) {
        return Err(invalid(format!("{field}: one of {}", allowed.join(", "))));
    }
    Ok(v.to_owned())
}

fn clean_notes(value: &str) -> Result<String, ServerPoolError> {
    if value.chars().count() > NOTES_MAX {
        return Err(invalid("notes: at most 2000 characters"));
    }
    Ok(value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_user_never_become_ssh_options() {
        assert!(clean_host("-oProxyCommand=x").is_err());
        assert!(clean_host("a b").is_err());
        assert!(clean_host("web;rm").is_err());
        assert_eq!(clean_host(" web-1.example.com ").unwrap(), "web-1.example.com");
        assert_eq!(clean_host("2001:db8::1").unwrap(), "2001:db8::1");
        assert!(clean_user("-l").is_err());
        assert!(clean_user("root;x").is_err());
        assert_eq!(clean_user("deploy").unwrap(), "deploy");
    }

    #[test]
    fn host_key_is_normalised() {
        let raw = "web-1.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc comment\n\n# x\nssh-rsa AAAAB3Nza=";
        assert_eq!(
            clean_host_key(raw).unwrap(),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc\nssh-rsa AAAAB3Nza="
        );
        assert_eq!(clean_host_key("").unwrap(), "");
        assert!(clean_host_key("not a key").is_err());
        assert!(clean_host_key("ssh-ed25519 abc$(x)").is_err());
    }

    #[test]
    fn private_key_rules() {
        let ok = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----";
        assert!(clean_private_key(ok).unwrap().ends_with("-----\n"));
        assert!(clean_private_key("ssh-ed25519 AAAA").is_err());
        assert!(clean_private_key("").is_err());
        let enc = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nx\n-----END RSA PRIVATE KEY-----";
        assert!(clean_private_key(enc).is_err());
    }

    #[test]
    fn names_and_enums() {
        assert!(clean_name("Web").is_err());
        assert!(clean_name("-web").is_err());
        assert_eq!(clean_name("web-1").unwrap(), "web-1");
        assert!(clean_enum("prod", ENVIRONMENTS, "environment").is_err());
        assert_eq!(
            clean_labels(&["a".into(), " a ".into(), "b".into()]).unwrap(),
            "[\"a\",\"b\"]"
        );
    }
}
