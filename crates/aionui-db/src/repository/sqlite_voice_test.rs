use super::SqliteVoiceRepository;
use crate::error::DbError;
use crate::init_database_memory;
use crate::models::VoiceSettingsRow;
use crate::repository::voice::{
    CreateVoiceSessionParams, IVoiceAuditRepository, IVoiceSessionRepository, IVoiceSettingsRepository,
    NewVoiceAuditEntry, VoiceUsageDelta,
};
use sqlx::SqlitePool;

const USER: &str = "voice-user-1";
const OTHER_USER: &str = "voice-user-2";

async fn repo() -> (SqliteVoiceRepository, crate::Database) {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), USER).await;
    seed_user(db.pool(), OTHER_USER).await;
    (SqliteVoiceRepository::new(db.pool().clone()), db)
}

async fn seed_user(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

fn params<'a>(id: &'a str, user: &'a str) -> CreateVoiceSessionParams<'a> {
    CreateVoiceSessionParams {
        id,
        user_id: user,
        engine: "openai-realtime",
        model: "gpt-realtime-2.1",
        resume_of: None,
        device: Some("web"),
        instructions_hash: Some("abc"),
    }
}

fn settings(user: &str, enabled: bool) -> VoiceSettingsRow {
    VoiceSettingsRow {
        user_id: user.into(),
        enabled,
        engine: "openai-realtime".into(),
        model: "gpt-realtime-2.1-mini".into(),
        car_mode: false,
        voice: "marin".into(),
        eagerness: "auto".into(),
        daily_budget_micros: 2_000_000,
        provider_id: None,
        extra: "{}".into(),
        updated_at: 1,
    }
}

#[tokio::test]
async fn settings_absent_until_upserted_and_scoped_per_user() {
    let (repo, _db) = repo().await;
    assert!(IVoiceSettingsRepository::get(&repo, USER).await.unwrap().is_none());

    repo.upsert(&settings(USER, true)).await.unwrap();
    let mut changed = settings(USER, false);
    changed.daily_budget_micros = 7;
    repo.upsert(&changed).await.unwrap();

    let row = IVoiceSettingsRepository::get(&repo, USER).await.unwrap().unwrap();
    assert!(!row.enabled);
    assert_eq!(row.daily_budget_micros, 7);
    assert!(
        IVoiceSettingsRepository::get(&repo, OTHER_USER)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn settings_default_row_has_voice_disabled() {
    let (repo, db) = repo().await;
    sqlx::query("INSERT INTO voice_settings (user_id, updated_at) VALUES (?, 1)")
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
    let row = IVoiceSettingsRepository::get(&repo, USER).await.unwrap().unwrap();
    assert!(!row.enabled, "voice must be off by default (kill switch)");
    assert_eq!(row.daily_budget_micros, 5_000_000);
    assert_eq!(row.model, "gpt-realtime-2.1");
}

#[tokio::test]
async fn one_live_session_per_user_is_enforced() {
    let (repo, _db) = repo().await;
    repo.create_starting(params("vs_1", USER)).await.unwrap();

    let err = repo.create_starting(params("vs_2", USER)).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "got {err:?}");

    // Another user is unaffected.
    repo.create_starting(params("vs_3", OTHER_USER)).await.unwrap();

    // After the first ends, a new one is admitted.
    assert!(repo.finish(USER, "vs_1", "ended", "user").await.unwrap());
    repo.create_starting(params("vs_2", USER)).await.unwrap();
}

#[tokio::test]
async fn lifecycle_usage_and_idempotent_finish() {
    let (repo, _db) = repo().await;
    repo.create_starting(params("vs_1", USER)).await.unwrap();
    repo.mark_live(USER, "vs_1", "rtc_abc").await.unwrap();
    // mark_live only from starting.
    assert!(matches!(
        repo.mark_live(USER, "vs_1", "rtc_other").await.unwrap_err(),
        DbError::NotFound(_)
    ));
    repo.mark_ready(USER, "vs_1", 42).await.unwrap();
    let delta = VoiceUsageDelta {
        input_tokens: 100,
        cached_input_tokens: 10,
        output_tokens: 50,
        responses: 1,
        unknown_responses: 0,
        stt_seconds: 3,
        cost_micros: 1234,
    };
    repo.add_usage(USER, "vs_1", &delta, 1).await.unwrap();
    repo.add_usage(USER, "vs_1", &delta, 1).await.unwrap();

    let row = IVoiceSessionRepository::get(&repo, USER, "vs_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "live");
    assert_eq!(row.call_id.as_deref(), Some("rtc_abc"));
    assert_eq!(row.ready_at, Some(42));
    assert_eq!(row.input_tokens, 200);
    assert_eq!(row.cost_micros, 2468);
    assert_eq!(row.stt_seconds, 6);

    assert_eq!(repo.list_open().await.unwrap().len(), 1);
    assert!(repo.finish(USER, "vs_1", "ended", "idle").await.unwrap());
    assert!(!repo.finish(USER, "vs_1", "failed", "late").await.unwrap());
    let row = IVoiceSessionRepository::get(&repo, USER, "vs_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "ended");
    assert_eq!(row.end_reason.as_deref(), Some("idle"));
    assert!(repo.list_open().await.unwrap().is_empty());
    assert!(repo.finish(USER, "vs_1", "bogus", "x").await.is_err());
}

#[tokio::test]
async fn sessions_are_user_scoped_and_cost_is_summed() {
    let (repo, _db) = repo().await;
    repo.create_starting(params("vs_1", USER)).await.unwrap();
    let delta = VoiceUsageDelta {
        cost_micros: 500,
        ..Default::default()
    };
    repo.add_usage(USER, "vs_1", &delta, 1).await.unwrap();
    // Cross-user writes and reads do nothing.
    repo.add_usage(OTHER_USER, "vs_1", &delta, 1).await.unwrap();
    assert!(
        IVoiceSessionRepository::get(&repo, OTHER_USER, "vs_1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!repo.finish(OTHER_USER, "vs_1", "ended", "x").await.unwrap());

    assert_eq!(repo.cost_since(USER, 0).await.unwrap(), 500);
    assert_eq!(repo.cost_since(USER, 2 * DAY).await.unwrap(), 0);
    assert_eq!(repo.cost_since(OTHER_USER, 0).await.unwrap(), 0);
    assert_eq!(IVoiceSessionRepository::list(&repo, USER, 10).await.unwrap().len(), 1);
    assert!(
        IVoiceSessionRepository::list(&repo, OTHER_USER, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

const DAY: i64 = 86_400_000;

/// A call that crosses UTC midnight is billed to the day the usage happened.
#[tokio::test]
async fn cost_is_attributed_to_the_day_it_was_charged() {
    let (repo, _db) = repo().await;
    repo.create_starting(params("vs_1", USER)).await.unwrap();
    let cost = |micros| VoiceUsageDelta {
        cost_micros: micros,
        ..Default::default()
    };
    // Started on day 10, 23:59; usage before and after midnight.
    repo.add_usage(USER, "vs_1", &cost(300), 10 * DAY + DAY - 1_000)
        .await
        .unwrap();
    repo.add_usage(USER, "vs_1", &cost(200), 11 * DAY + 5_000)
        .await
        .unwrap();
    repo.add_usage(USER, "vs_1", &cost(50), 11 * DAY + 9_000).await.unwrap();
    assert_eq!(repo.cost_since(USER, 10 * DAY).await.unwrap(), 550);
    assert_eq!(
        repo.cost_since(USER, 11 * DAY + 123).await.unwrap(),
        250,
        "today = day 11 only"
    );
    assert_eq!(repo.cost_since(USER, 12 * DAY).await.unwrap(), 0);
    let row = IVoiceSessionRepository::get(&repo, USER, "vs_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.cost_micros, 550, "the session total is unchanged");
}

#[tokio::test]
async fn audit_appends_and_lists_newest_first() {
    let (repo, _db) = repo().await;
    for event in ["session_created", "session_ended"] {
        repo.append(NewVoiceAuditEntry {
            user_id: USER,
            session_id: Some("vs_1"),
            event,
            detail: r#"{"reason":"user"}"#,
        })
        .await
        .unwrap();
    }
    let rows = IVoiceAuditRepository::list(&repo, USER, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].event, "session_ended");
    assert!(
        IVoiceAuditRepository::list(&repo, OTHER_USER, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let bad = repo
        .append(NewVoiceAuditEntry {
            user_id: USER,
            session_id: None,
            event: "x",
            detail: "not json",
        })
        .await;
    assert!(bad.is_err(), "detail must be valid JSON");
}
