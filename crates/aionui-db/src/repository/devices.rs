//! C38 devices registry (migration 911): devices, relay pairings, audit.

use crate::error::DbError;
use crate::models::{DeviceAuditRow, DevicePairingRow, DeviceRow};

/// Upsert of a device seen in a relay client's inventory (or an add-on).
/// Keyed by `(user_id, kind, pairing_id, source_id)`; an existing row keeps
/// its id, switch and timeout and only refreshes name, meta and last seen.
#[derive(Debug, Clone)]
pub struct UpsertDeviceParams<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub kind: &'a str,
    pub pairing_id: Option<&'a str>,
    pub source_id: &'a str,
    pub name: &'a str,
    pub meta: &'a str,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct NewDeviceAuditEntry<'a> {
    pub user_id: &'a str,
    pub device_id: &'a str,
    pub at: i64,
    pub verb: &'a str,
    pub package: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub outcome: &'a str,
    pub conversation_id: Option<&'a str>,
    pub detail: Option<&'a str>,
}

#[async_trait::async_trait]
pub trait IDeviceRepository: Send + Sync {
    /// Insert or refresh; returns the stored row.
    async fn upsert_device(&self, params: UpsertDeviceParams<'_>) -> Result<DeviceRow, DbError>;
    async fn get_device(&self, user_id: &str, id: &str) -> Result<Option<DeviceRow>, DbError>;
    async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceRow>, DbError>;
    /// The agent switch (one statement). Returns false when no row matched.
    async fn set_device_enabled(
        &self,
        user_id: &str,
        id: &str,
        enabled: bool,
        enabled_until: Option<i64>,
        now: i64,
    ) -> Result<bool, DbError>;
    /// Turn off every switch whose timeout is at or before `now`; returns the
    /// rows switched off (startup reconcile and the timeout ticker).
    async fn expire_device_switches(&self, now: i64) -> Result<Vec<DeviceRow>, DbError>;
    async fn delete_device(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    async fn insert_pairing(&self, row: &DevicePairingRow) -> Result<(), DbError>;
    /// An active (not revoked) pairing by token hash, any user.
    async fn find_pairing_by_token_hash(&self, token_sha256: &str) -> Result<Option<DevicePairingRow>, DbError>;
    async fn list_pairings(&self, user_id: &str) -> Result<Vec<DevicePairingRow>, DbError>;
    /// Record a connection: version, capabilities, last seen.
    async fn touch_pairing(&self, id: &str, version: &str, capabilities: &str, now: i64) -> Result<(), DbError>;
    /// Revoke a pairing of `user_id` and switch off the devices it reached.
    /// Returns false when no active pairing matched.
    async fn revoke_pairing(&self, user_id: &str, id: &str, now: i64) -> Result<bool, DbError>;

    async fn append_audit(&self, entry: NewDeviceAuditEntry<'_>) -> Result<i64, DbError>;
    /// Newest first: rows with `id < before` (all when `None`), at most `limit`.
    async fn list_audit(
        &self,
        user_id: &str,
        device_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<DeviceAuditRow>, DbError>;
}
