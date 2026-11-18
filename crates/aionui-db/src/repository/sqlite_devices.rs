use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{DeviceAuditRow, DevicePairingRow, DeviceRow};
use crate::repository::devices::{IDeviceRepository, NewDeviceAuditEntry, UpsertDeviceParams};

/// SQLite implementation of [`IDeviceRepository`] (migration 911).
#[derive(Clone, Debug)]
pub struct SqliteDeviceRepository {
    pool: SqlitePool,
}

impl SqliteDeviceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const DEVICE_COLUMNS: &str = "id, user_id, kind, pairing_id, source_id, name, enabled, enabled_until, meta, created_at, updated_at, last_seen_at";

#[async_trait::async_trait]
impl IDeviceRepository for SqliteDeviceRepository {
    async fn upsert_device(&self, p: UpsertDeviceParams<'_>) -> Result<DeviceRow, DbError> {
        sqlx::query(
            "INSERT INTO devices (id, user_id, kind, pairing_id, source_id, name, enabled, meta, created_at, \
             updated_at, last_seen_at) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?) \
             ON CONFLICT(user_id, kind, COALESCE(pairing_id, ''), source_id) DO UPDATE SET \
             name = excluded.name, \
             meta = json_patch(excluded.meta, json_object('access', json(json_extract(devices.meta, '$.access')))), \
             updated_at = excluded.updated_at, \
             last_seen_at = excluded.last_seen_at",
        )
        .bind(p.id)
        .bind(p.user_id)
        .bind(p.kind)
        .bind(p.pairing_id)
        .bind(p.source_id)
        .bind(p.name)
        .bind(p.meta)
        .bind(p.now)
        .bind(p.now)
        .bind(p.now)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query_as::<_, DeviceRow>(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices WHERE user_id = ? AND kind = ? \
             AND COALESCE(pairing_id, '') = COALESCE(?, '') AND source_id = ?"
        ))
        .bind(p.user_id)
        .bind(p.kind)
        .bind(p.pairing_id)
        .bind(p.source_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_device(&self, user_id: &str, id: &str) -> Result<Option<DeviceRow>, DbError> {
        Ok(sqlx::query_as::<_, DeviceRow>(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices WHERE user_id = ? AND id = ?"
        ))
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceRow>, DbError> {
        Ok(sqlx::query_as::<_, DeviceRow>(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices WHERE user_id = ? ORDER BY kind, name, id"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_device_enabled(
        &self,
        user_id: &str,
        id: &str,
        enabled: bool,
        enabled_until: Option<i64>,
        now: i64,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE devices SET enabled = ?, enabled_until = ?, updated_at = ? WHERE user_id = ? AND id = ?",
        )
        .bind(enabled)
        .bind(if enabled { enabled_until } else { None })
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn set_device_access(&self, user_id: &str, id: &str, access_json: &str, now: i64) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE devices SET meta = json_set(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, \
             '$.access', json(?)), updated_at = ? WHERE user_id = ? AND id = ?",
        )
        .bind(access_json)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn expire_device_switches(&self, now: i64) -> Result<Vec<DeviceRow>, DbError> {
        // ONE write statement, no read-then-write transaction: a deferred
        // transaction that reads first cannot wait for another writer when it
        // upgrades (SQLite answers "database is locked" at once, whatever the
        // busy timeout), which made this fail whenever AionCore was writing.
        let rows = sqlx::query_as::<_, DeviceRow>(&format!(
            "UPDATE devices SET enabled = 0, enabled_until = NULL, updated_at = ? \
             WHERE enabled = 1 AND enabled_until IS NOT NULL AND enabled_until <= ? \
             RETURNING {DEVICE_COLUMNS}"
        ))
        .bind(now)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete_device(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let done = sqlx::query("DELETE FROM devices WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn insert_pairing(&self, row: &DevicePairingRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO device_pairings (id, user_id, kind, name, token_sha256, version, capabilities, \
             created_at, last_seen_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.kind)
        .bind(&row.name)
        .bind(&row.token_sha256)
        .bind(&row.version)
        .bind(&row.capabilities)
        .bind(row.created_at)
        .bind(row.last_seen_at)
        .bind(row.revoked_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_pairing_by_token_hash(&self, token_sha256: &str) -> Result<Option<DevicePairingRow>, DbError> {
        Ok(sqlx::query_as::<_, DevicePairingRow>(
            "SELECT * FROM device_pairings WHERE token_sha256 = ? AND revoked_at IS NULL",
        )
        .bind(token_sha256)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_pairings(&self, user_id: &str) -> Result<Vec<DevicePairingRow>, DbError> {
        Ok(sqlx::query_as::<_, DevicePairingRow>(
            "SELECT * FROM device_pairings WHERE user_id = ? AND revoked_at IS NULL ORDER BY created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn touch_pairing(&self, id: &str, version: &str, capabilities: &str, now: i64) -> Result<(), DbError> {
        sqlx::query("UPDATE device_pairings SET version = ?, capabilities = ?, last_seen_at = ? WHERE id = ?")
            .bind(version)
            .bind(capabilities)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke_pairing(&self, user_id: &str, id: &str, now: i64) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE device_pairings SET revoked_at = ? WHERE user_id = ? AND id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE devices SET enabled = 0, enabled_until = NULL, updated_at = ? WHERE user_id = ? AND pairing_id = ?",
        )
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn append_audit(&self, e: NewDeviceAuditEntry<'_>) -> Result<i64, DbError> {
        let done = sqlx::query(
            "INSERT INTO device_audit (user_id, device_id, at, verb, package, origin, outcome, conversation_id, \
             detail) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(e.user_id)
        .bind(e.device_id)
        .bind(e.at)
        .bind(e.verb)
        .bind(e.package)
        .bind(e.origin)
        .bind(e.outcome)
        .bind(e.conversation_id)
        .bind(e.detail)
        .execute(&self.pool)
        .await?;
        Ok(done.last_insert_rowid())
    }

    async fn list_audit(
        &self,
        user_id: &str,
        device_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<DeviceAuditRow>, DbError> {
        Ok(sqlx::query_as::<_, DeviceAuditRow>(
            "SELECT * FROM device_audit WHERE user_id = ? AND device_id = ? AND (? IS NULL OR id < ?) \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(before)
        .bind(before)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    fn upsert<'a>(id: &'a str, user: &'a str, serial: &'a str, name: &'a str, now: i64) -> UpsertDeviceParams<'a> {
        UpsertDeviceParams {
            id,
            user_id: user,
            kind: "phone",
            pairing_id: Some("p1"),
            source_id: serial,
            name,
            meta: "{}",
            now,
        }
    }

    #[tokio::test]
    async fn devices_upsert_keep_id_and_switch_and_expire() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteDeviceRepository::new(db.pool().clone());
        let first = repo
            .upsert_device(upsert("phone:a", "u1", "R5CX", "Galaxy", 1))
            .await
            .unwrap();
        assert_eq!((first.id.as_str(), first.enabled), ("phone:a", false));
        assert!(
            repo.set_device_enabled("u1", "phone:a", true, Some(100), 2)
                .await
                .unwrap()
        );
        assert!(
            !repo.set_device_enabled("u2", "phone:a", true, None, 2).await.unwrap(),
            "other user"
        );
        // The same phone again from the inventory: same row, switch kept.
        let again = repo
            .upsert_device(upsert("phone:b", "u1", "R5CX", "Galaxy S24", 3))
            .await
            .unwrap();
        assert_eq!(again.id, "phone:a");
        assert_eq!(
            (again.name.as_str(), again.enabled, again.enabled_until),
            ("Galaxy S24", true, Some(100))
        );
        assert!(repo.expire_device_switches(99).await.unwrap().is_empty());
        let expired = repo.expire_device_switches(100).await.unwrap();
        assert_eq!(expired.len(), 1);
        let row = repo.get_device("u1", "phone:a").await.unwrap().unwrap();
        assert_eq!((row.enabled, row.enabled_until), (false, None));
        assert!(repo.get_device("u2", "phone:a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pairings_are_found_by_hash_and_revoke_switches_devices_off() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteDeviceRepository::new(db.pool().clone());
        repo.insert_pairing(&DevicePairingRow {
            id: "p1".into(),
            user_id: "u1".into(),
            kind: "mac_helper".into(),
            name: "MacBook".into(),
            token_sha256: "h1".into(),
            version: None,
            capabilities: "[]".into(),
            created_at: 1,
            last_seen_at: None,
            revoked_at: None,
        })
        .await
        .unwrap();
        repo.upsert_device(upsert("phone:a", "u1", "R5CX", "Galaxy", 1))
            .await
            .unwrap();
        repo.set_device_enabled("u1", "phone:a", true, None, 2).await.unwrap();
        assert_eq!(repo.find_pairing_by_token_hash("h1").await.unwrap().unwrap().id, "p1");
        assert!(
            !repo.revoke_pairing("u2", "p1", 3).await.unwrap(),
            "another user cannot revoke"
        );
        assert!(repo.revoke_pairing("u1", "p1", 3).await.unwrap());
        assert!(repo.find_pairing_by_token_hash("h1").await.unwrap().is_none());
        assert!(!repo.get_device("u1", "phone:a").await.unwrap().unwrap().enabled);
        assert!(!repo.revoke_pairing("u1", "p1", 4).await.unwrap(), "already revoked");
    }

    #[tokio::test]
    async fn audit_pages_newest_first_per_user() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteDeviceRepository::new(db.pool().clone());
        for (i, outcome) in ["ok", "refused", "approved"].iter().enumerate() {
            repo.append_audit(NewDeviceAuditEntry {
                user_id: "u1",
                device_id: "phone:a",
                at: i as i64,
                verb: "tap",
                package: Some("com.app"),
                origin: None,
                outcome,
                conversation_id: None,
                detail: None,
            })
            .await
            .unwrap();
        }
        let page = repo.list_audit("u1", "phone:a", None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
            ["approved", "refused"]
        );
        let rest = repo.list_audit("u1", "phone:a", Some(page[1].id), 2).await.unwrap();
        assert_eq!(rest.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(), ["ok"]);
        assert!(repo.list_audit("u2", "phone:a", None, 10).await.unwrap().is_empty());
    }
}
