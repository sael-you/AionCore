//! Board repository (migration 901): TASK-N under concurrency, ordering and
//! rebalance, version guards, user scoping, event paging, project delete.

use std::sync::Arc;

use aionui_db::{
    BOARD_POSITION_GAP, BoardCardRow, BoardColumnRow, BoardEventQuery, BoardEventScope, BoardPlacement,
    BoardPlacementOutcome, BoardSettingsRow, IBoardStore, NewBoardEvent, SqliteBoardStore, SqlitePool, init_database,
    init_database_memory,
};

const USER: &str = "board-user";
const OTHER: &str = "board-other";
const PROJECT: &str = "proj-board";

async fn seed_user(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

fn settings(user: &str, project: &str) -> BoardSettingsRow {
    BoardSettingsRow {
        project_id: project.into(),
        user_id: user.into(),
        enabled: true,
        template: "studio".into(),
        keeper_conversation_id: None,
        last_event_seq: 0,
        next_number: 1,
        version: 1,
        settings: "{}".into(),
        archived_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn column(user: &str, project: &str, key: &str, position: i64) -> BoardColumnRow {
    BoardColumnRow {
        id: format!("col-{project}-{key}"),
        project_id: project.into(),
        user_id: user.into(),
        key: key.into(),
        name: None,
        category: "todo".into(),
        kind: "normal".into(),
        resolution: None,
        agent_move: "allow".into(),
        wip_limit: None,
        exit_rule: None,
        gate: None,
        position,
        archived: false,
    }
}

fn card(user: &str, project: &str, id: &str, number: i64, column: &str, position: i64) -> BoardCardRow {
    BoardCardRow {
        id: id.into(),
        project_id: project.into(),
        user_id: user.into(),
        number,
        title: format!("card {number}"),
        description: String::new(),
        card_type: "feature".into(),
        priority: "medium".into(),
        size: None,
        tier: None,
        column_key: column.into(),
        position,
        labels: r#"["ui"]"#.into(),
        skills: "[]".into(),
        routing: "[]".into(),
        creator_kind: "user".into(),
        creator_ref: user.into(),
        assignee_kind: None,
        assignee_ref: None,
        assignee_conversation_id: None,
        blocked: false,
        blocked_reason: None,
        blocked_by: None,
        change_id: None,
        metadata: "{}".into(),
        version: 1,
        archived_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn event(user: &str, project: &str, card_id: Option<&str>, kind: &str, id: &str) -> NewBoardEvent {
    NewBoardEvent {
        id: id.into(),
        card_id: card_id.map(str::to_owned),
        project_id: project.into(),
        user_id: user.into(),
        actor_kind: "user".into(),
        actor_ref: user.into(),
        conversation_id: None,
        kind: kind.into(),
        payload: "{}".into(),
        created_at: 1,
    }
}

async fn board(store: &SqliteBoardStore, user: &str, project: &str) {
    let mut tx = store.begin().await.unwrap();
    tx.insert_settings(&settings(user, project)).await.unwrap();
    tx.upsert_columns(&[column(user, project, "todo", 0), column(user, project, "done", 1)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn positions(store: &SqliteBoardStore, column: &str) -> Vec<(String, i64)> {
    store
        .cards(USER, PROJECT, false)
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.column_key == column)
        .map(|c| (c.id, c.position))
        .collect()
}

#[tokio::test]
async fn task_numbers_stay_unique_under_concurrent_writers() {
    // A file DB with a real connection pool, so BEGIN IMMEDIATE transactions
    // actually contend (the in-memory DB has a single connection).
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("board.db")).await.unwrap();
    let store = Arc::new(SqliteBoardStore::new(db.pool().clone()));
    board(&store, USER, PROJECT).await;

    let mut handles = Vec::new();
    for i in 0..24 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = store.begin().await.unwrap();
            let number = tx.claim_number(USER, PROJECT).await.unwrap().unwrap();
            let BoardPlacementOutcome::Placed(position) = tx
                .place(USER, PROJECT, "todo", None, BoardPlacement::Tail)
                .await
                .unwrap()
            else {
                panic!("tail placement");
            };
            tx.insert_card(&card(USER, PROJECT, &format!("c{i}"), number, "todo", position))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            (number, position)
        }));
    }
    let mut numbers = Vec::new();
    let mut seen_positions = Vec::new();
    for handle in handles {
        let (number, position) = handle.await.unwrap();
        numbers.push(number);
        seen_positions.push(position);
    }
    numbers.sort();
    assert_eq!(numbers, (1..=24).collect::<Vec<_>>(), "TASK-N is gap-free and unique");
    seen_positions.sort();
    seen_positions.dedup();
    assert_eq!(seen_positions.len(), 24, "tail placements never collide");
    assert_eq!(store.settings(USER, PROJECT).await.unwrap().unwrap().next_number, 25);
    db.close().await;
}

#[tokio::test]
async fn placement_uses_midpoints_and_rebalances_an_exhausted_gap() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "a", 1, "todo", 10)).await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "b", 2, "todo", 11)).await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "c", 3, "todo", 2000))
        .await
        .unwrap();
    tx.insert_card(&card(USER, PROJECT, "m", 4, "done", 1024))
        .await
        .unwrap();

    // Room between b (11) and c (2000): integer midpoint, no renumbering.
    assert_eq!(
        tx.place(USER, PROJECT, "todo", Some("m"), BoardPlacement::After("b"))
            .await
            .unwrap(),
        BoardPlacementOutcome::Placed(1005)
    );
    // Head insert: first - 1024.
    assert_eq!(
        tx.place(USER, PROJECT, "todo", Some("m"), BoardPlacement::Before("a"))
            .await
            .unwrap(),
        BoardPlacementOutcome::Placed(10 - BOARD_POSITION_GAP)
    );
    // a (10) / b (11): gap < 2 → the column is renumbered first.
    let outcome = tx
        .place(USER, PROJECT, "todo", Some("m"), BoardPlacement::After("a"))
        .await
        .unwrap();
    assert_eq!(outcome, BoardPlacementOutcome::Placed(1024 + 512));
    // An anchor outside the target column is reported, not guessed.
    assert_eq!(
        tx.place(USER, PROJECT, "todo", Some("m"), BoardPlacement::After("m"))
            .await
            .unwrap(),
        BoardPlacementOutcome::AnchorNotFound
    );
    tx.commit().await.unwrap();
    assert_eq!(
        positions(&store, "todo").await,
        vec![("a".into(), 1024), ("b".into(), 2048), ("c".into(), 3072)],
        "rebalanced to 1024 spacing in (position, id) order"
    );
    assert_eq!(
        positions(&store, "done").await,
        vec![("m".into(), 1024)],
        "other columns untouched"
    );
}

#[tokio::test]
async fn equal_positions_tie_break_by_id_and_force_a_rebalance() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "y", 1, "todo", 5)).await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "x", 2, "todo", 5)).await.unwrap();
    assert_eq!(
        tx.place(USER, PROJECT, "todo", None, BoardPlacement::After("x"))
            .await
            .unwrap(),
        BoardPlacementOutcome::Placed(1536)
    );
    tx.commit().await.unwrap();
    assert_eq!(
        positions(&store, "todo").await,
        vec![("x".into(), 1024), ("y".into(), 2048)]
    );
}

#[tokio::test]
async fn versions_guard_cards_settings_and_questions() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "a", 1, "todo", 1024))
        .await
        .unwrap();
    let mut row = tx.card(USER, "a").await.unwrap().unwrap();
    row.title = "renamed".into();
    row.version = 2;
    assert!(!tx.update_card(&row, 7).await.unwrap(), "stale version rejected");
    assert!(tx.update_card(&row, 1).await.unwrap());
    assert!(
        !tx.update_card(&row, 1).await.unwrap(),
        "the same base version cannot win twice"
    );

    let mut header = tx.settings(USER, PROJECT).await.unwrap().unwrap();
    header.enabled = false;
    header.version = 2;
    assert!(!tx.update_settings(&header, 5).await.unwrap());
    assert!(tx.update_settings(&header, 1).await.unwrap());
    tx.commit().await.unwrap();

    sqlx::query(
        "INSERT INTO board_card_questions (id, card_id, project_id, user_id, from_department, to_department, text, \
         created_at, updated_at) VALUES ('q1', 'a', ?, ?, 'review', 'owner', 'why?', 1, 1)",
    )
    .bind(PROJECT)
    .bind(USER)
    .execute(db.pool())
    .await
    .unwrap();
    let mut tx = store.begin().await.unwrap();
    let mut question = tx.question(USER, "a", "q1").await.unwrap().unwrap();
    assert_eq!(question.version, 1, "questions start at version 1");
    question.answered_by_event = Some("ev".into());
    question.version = 2;
    assert!(!tx.update_question(&question, 2).await.unwrap());
    assert!(tx.update_question(&question, 1).await.unwrap());
    tx.commit().await.unwrap();

    let stored = store.card(USER, "a").await.unwrap().unwrap();
    assert_eq!((stored.title.as_str(), stored.version), ("renamed", 2));
    assert!(!store.settings(USER, PROJECT).await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn reads_and_writes_are_scoped_to_the_owner() {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), USER).await;
    seed_user(db.pool(), OTHER).await;
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "a", 1, "todo", 1024))
        .await
        .unwrap();
    tx.insert_event(&event(USER, PROJECT, Some("a"), "card_created", "e1"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(store.settings(OTHER, PROJECT).await.unwrap().is_none());
    assert!(store.columns(OTHER, PROJECT).await.unwrap().is_empty());
    assert!(store.card(OTHER, "a").await.unwrap().is_none());
    assert!(store.card_by_number(OTHER, PROJECT, 1).await.unwrap().is_none());
    assert!(store.cards(OTHER, PROJECT, true).await.unwrap().is_empty());
    let query = BoardEventQuery {
        scope: BoardEventScope::Card("a"),
        after_seq: 0,
        limit: 10,
        kinds: None,
    };
    assert!(store.events(OTHER, &query).await.unwrap().is_empty());
    assert_eq!(store.events(USER, &query).await.unwrap().len(), 1);

    let mut tx = store.begin().await.unwrap();
    assert!(
        tx.claim_number(OTHER, PROJECT).await.unwrap().is_none(),
        "no foreign counter"
    );
    let mut foreign = card(USER, PROJECT, "a", 1, "todo", 1024);
    foreign.user_id = OTHER.into();
    foreign.title = "hijacked".into();
    assert!(
        !tx.update_card(&foreign, 1).await.unwrap(),
        "foreign owner matches no row"
    );
    tx.commit().await.unwrap();
    assert_eq!(store.card(USER, "a").await.unwrap().unwrap().title, "card 1");
    assert_eq!(store.delete_board(OTHER, PROJECT).await.unwrap(), 0);
    assert!(store.settings(USER, PROJECT).await.unwrap().is_some());
}

#[tokio::test]
async fn event_pages_are_keyset_by_seq_and_filterable_by_kind() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    let mut seqs = Vec::new();
    for (i, kind) in ["card_created", "commented", "card_moved", "commented"]
        .iter()
        .enumerate()
    {
        seqs.push(
            tx.insert_event(&event(USER, PROJECT, Some("a"), kind, &format!("e{i}")))
                .await
                .unwrap()
                .seq,
        );
    }
    tx.commit().await.unwrap();
    assert!(seqs.windows(2).all(|w| w[1] > w[0]), "seq is monotonic");

    let page = |after_seq, kinds| BoardEventQuery {
        scope: BoardEventScope::Project(PROJECT),
        after_seq,
        limit: 2,
        kinds,
    };
    let first = store.events(USER, &page(0, None)).await.unwrap();
    assert_eq!(first.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e0", "e1"]);
    let second = store.events(USER, &page(first[1].seq, None)).await.unwrap();
    assert_eq!(second.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e2", "e3"]);
    let comments = store.events(USER, &page(0, Some(&["commented"]))).await.unwrap();
    assert_eq!(comments.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1", "e3"]);
    let recent = store.recent_card_events(USER, "a", 2).await.unwrap();
    assert_eq!(
        recent.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["e2", "e3"],
        "newest, ascending"
    );

    let mut tx = store.begin().await.unwrap();
    let duplicate = tx.insert_event(&event(USER, PROJECT, None, "commented", "e0")).await;
    assert!(duplicate.is_err(), "event ids are unique");
}

#[tokio::test]
async fn delete_board_removes_every_board_row_of_the_project_only() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    board(&store, USER, "proj-keep").await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_card(&card(USER, PROJECT, "a", 1, "todo", 1024))
        .await
        .unwrap();
    tx.insert_card(&card(USER, "proj-keep", "k", 1, "todo", 1024))
        .await
        .unwrap();
    tx.insert_event(&event(USER, PROJECT, Some("a"), "card_created", "e1"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(store.count_cards(USER, PROJECT, true).await.unwrap(), 1);
    assert_eq!(store.delete_board(USER, PROJECT).await.unwrap(), 1);
    assert!(store.settings(USER, PROJECT).await.unwrap().is_none());
    assert!(store.columns(USER, PROJECT).await.unwrap().is_empty());
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM board_events WHERE project_id = ?")
        .bind(PROJECT)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(store.card(USER, "k").await.unwrap().is_some(), "other boards untouched");
}

#[tokio::test]
async fn paged_cards_filter_by_label_and_resume_from_the_cursor() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    let mut tx = store.begin().await.unwrap();
    for (i, column) in ["todo", "todo", "done"].iter().enumerate() {
        let mut row = card(
            USER,
            PROJECT,
            &format!("c{i}"),
            i as i64 + 1,
            column,
            1024 * (i as i64 + 1),
        );
        if i == 1 {
            row.labels = r#"["api"]"#.into();
        }
        tx.insert_card(&row).await.unwrap();
    }
    tx.commit().await.unwrap();
    let filter = aionui_db::BoardCardFilter::default();
    let first = store.cards_page(USER, PROJECT, &filter, None, 2).await.unwrap();
    assert_eq!(
        first.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["c2", "c0"],
        "(column, position, id)"
    );
    let last = &first[1];
    let cursor = aionui_db::BoardCardCursor {
        column_key: last.column_key.clone(),
        position: last.position,
        id: last.id.clone(),
    };
    let next = store
        .cards_page(USER, PROJECT, &filter, Some(&cursor), 2)
        .await
        .unwrap();
    assert_eq!(next.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c1"]);
    let labelled = aionui_db::BoardCardFilter {
        label: Some("api".into()),
        ..Default::default()
    };
    let api = store.cards_page(USER, PROJECT, &labelled, None, 10).await.unwrap();
    assert_eq!(api.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c1"]);
}

#[tokio::test]
async fn each_project_chains_its_events_without_gaps_when_interleaved() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteBoardStore::new(db.pool().clone());
    board(&store, USER, PROJECT).await;
    board(&store, USER, "proj-other").await;
    let mut tx = store.begin().await.unwrap();
    for i in 0..6 {
        let project = if i % 2 == 0 { PROJECT } else { "proj-other" };
        tx.insert_event(&event(USER, project, None, "commented", &format!("x{i}")))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    for project in [PROJECT, "proj-other"] {
        let rows = store
            .events(
                USER,
                &BoardEventQuery {
                    scope: BoardEventScope::Project(project),
                    after_seq: 0,
                    limit: 100,
                    kinds: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].prev_seq, 0, "first event of the project");
        for pair in rows.windows(2) {
            assert_eq!(pair[1].prev_seq, pair[0].seq, "{project}: chain has no gap");
        }
        assert_ne!(rows[1].seq, rows[0].seq + 1, "global seq is interleaved");
        assert_eq!(
            store.last_seq(USER, project).await.unwrap(),
            rows[2].seq,
            "head = last_event_seq"
        );
    }
}
