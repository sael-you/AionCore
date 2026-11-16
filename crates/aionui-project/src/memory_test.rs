use std::path::{Path, PathBuf};

use super::*;

const WORKSPACE: &str = "/root/projects/TB-test";

/// A temp `.claude/projects` root with the workspace's memory dir.
async fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, MemoryService, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let service = MemoryService::new(root.path());
    let dir = service.dir_for(Path::new(WORKSPACE));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    for (name, text) in files {
        tokio::fs::write(dir.join(name), text).await.unwrap();
    }
    (root, service, dir)
}

fn memory(name: &str, kind: &str, body: &str) -> String {
    memory_at(name, kind, body, "2026-11-11T01:13:40.790Z")
}

fn memory_at(name: &str, kind: &str, body: &str, modified: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: \"About {name}\"\nmetadata:\n  node_type: memory\n  type: {kind}\n  \
         modified: {modified}\n---\n\n{body}\n"
    )
}

fn ws() -> &'static Path {
    Path::new(WORKSPACE)
}

#[test]
fn the_slug_matches_claude_codes_folder_names() {
    assert_eq!(slug(Path::new("/root/projects/TB-test")), "-root-projects-TB-test");
    assert_eq!(slug(Path::new("/root/.clauterm/scratch")), "-root--clauterm-scratch");
    assert_eq!(
        slug(Path::new("/data/users/system_default_user/x")),
        "-data-users-system-default-user-x"
    );
    let service = MemoryService::new("/home/u/.claude/projects");
    assert_eq!(
        service.dir_for(ws()),
        PathBuf::from("/home/u/.claude/projects/-root-projects-TB-test/memory")
    );
}

#[test]
fn file_names_are_strict() {
    for ok in ["feedback_tests.md", "a-b.c.md", "MEMORY.md"] {
        assert!(valid_file_name(ok), "{ok}");
    }
    for bad in [
        "../x.md",
        "a/b.md",
        ".hidden.md",
        "x.txt",
        ".md",
        "a b.md",
        "..%2fx.md",
        "x.md/",
    ] {
        assert!(!valid_file_name(bad), "{bad}");
    }
}

#[test]
fn frontmatter_is_split_and_parsed_and_malformed_is_kept_raw() {
    let text = memory(
        "tests-first",
        "feedback",
        "Body with [[other]] and [[other]] and [[third]].",
    );
    let (raw, body) = split_frontmatter(&text);
    assert!(raw.starts_with("name: tests-first"));
    assert!(body.starts_with("Body with"));
    let front = parse_front(&raw);
    assert_eq!(front.name.as_deref(), Some("tests-first"));
    assert_eq!(front.description.as_deref(), Some("About tests-first"));
    assert_eq!(front.kind.as_deref(), Some("feedback"));
    assert_eq!(front.modified.as_deref(), Some("2026-11-11T01:13:40.790Z"));
    assert_eq!(links(&body), vec!["other".to_owned(), "third".to_owned()]);

    // No closing line: no frontmatter, everything is body.
    let (raw, body) = split_frontmatter("---\nname: x\nno end");
    assert_eq!(raw, "");
    assert_eq!(body, "---\nname: x\nno end");
    // Garbage frontmatter: fields None, raw kept.
    let (raw, _) = split_frontmatter("---\n: : :\n[[not yaml\n---\nbody");
    assert_eq!(raw, ": : :\n[[not yaml");
    assert_eq!(parse_front(&raw), Front::default());
}

#[tokio::test]
async fn a_project_without_a_memory_dir_has_no_memory_yet() {
    let root = tempfile::tempdir().unwrap();
    let service = MemoryService::new(root.path());
    let list = service.list(ws()).await.unwrap();
    assert!(!list.dir_exists);
    assert!(list.items.is_empty() && list.lint.is_empty());
    assert_eq!(service.read(ws(), "x.md").await, Err(MemoryError::NoMemory));
}

#[tokio::test]
async fn list_and_lint_find_duplicates_orphans_missing_entries_and_broken_links() {
    let index = "# Memory Index\n\
        - [Tests first](feedback_tests.md) — hook\n\
        - [Tests first again](feedback_tests_dupe.md) — dupe\n\
        - [Gone](deleted_long_ago.md) — points nowhere\n\
        - [Docs](https://example.com/x.md) — not a memory file\n";
    let (_root, service, _dir) = fixture(&[
        ("MEMORY.md", index),
        (
            "feedback_tests.md",
            &memory("tests-first", "feedback", "See [[deploy-rule]] and [[nowhere]]."),
        ),
        (
            "feedback_tests_dupe.md",
            &memory("Tests First", "feedback", "Same rule."),
        ),
        (
            "project_deploy.md",
            &memory_at("deploy-rule", "project", "Old project fact.", "2020-01-01T00:00:00Z"),
        ),
        (
            "project_fresh.md",
            &memory_at("fresh", "project", "New fact.", &chrono::Utc::now().to_rfc3339()),
        ),
        ("broken_front.md", "---\n: : :\n---\nNo usable fields."),
        ("notes.txt", "ignored"),
    ])
    .await;
    let list = service.list(ws()).await.unwrap();
    assert!(list.dir_exists);
    assert_eq!(list.index_lines, 5);
    let files: Vec<&str> = list.items.iter().map(|i| i.file.as_str()).collect();
    assert_eq!(files.len(), 5, "MEMORY.md and non-.md files are not items: {files:?}");

    let item = |f: &str| list.items.iter().find(|i| i.file == f).unwrap();
    let tests = item("feedback_tests.md");
    assert_eq!(tests.name.as_deref(), Some("tests-first"));
    assert_eq!(tests.kind.as_deref(), Some("feedback"));
    assert!(tests.in_index);
    assert_eq!(tests.links, vec!["deploy-rule".to_owned(), "nowhere".to_owned()]);
    assert_eq!(tests.broken_links, vec!["nowhere".to_owned()]);
    assert!(tests.size > 0 && tests.modified > 0);
    let broken = item("broken_front.md");
    assert_eq!(
        (broken.name.as_ref(), broken.kind.as_ref()),
        (None, None),
        "malformed: listed, fields None"
    );
    assert!(item("project_deploy.md").stale, "a project memory older than 30 days");
    assert!(!item("project_fresh.md").stale);
    assert!(!item("feedback_tests_dupe.md").stale, "only project memories go stale");

    let kinds = |kind: &str| -> Vec<Option<&str>> {
        list.lint
            .iter()
            .filter(|l| l.kind == kind)
            .map(|l| l.file.as_deref())
            .collect()
    };
    assert_eq!(
        kinds("duplicate"),
        vec![Some("feedback_tests.md"), Some("feedback_tests_dupe.md")]
    );
    let mut orphans = kinds("not_in_index");
    orphans.sort();
    assert_eq!(
        orphans,
        vec![
            Some("broken_front.md"),
            Some("project_deploy.md"),
            Some("project_fresh.md")
        ]
    );
    assert_eq!(kinds("stale"), vec![Some("project_deploy.md")]);
    assert_eq!(kinds("missing_file"), vec![Some("deleted_long_ago.md")]);
    assert_eq!(kinds("broken_link"), vec![Some("feedback_tests.md")]);
}

#[tokio::test]
async fn path_traversal_and_symlinks_are_refused() {
    let (root, service, dir) = fixture(&[("ok.md", "x")]).await;
    let outside = root.path().join("secret.md");
    tokio::fs::write(&outside, "secret").await.unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link.md")).unwrap();
    for bad in ["../secret.md", "../../secret.md", "/etc/passwd", ".trash"] {
        assert_eq!(service.read(ws(), bad).await, Err(MemoryError::InvalidName), "{bad}");
    }
    assert_eq!(
        service.read(ws(), "link.md").await,
        Err(MemoryError::NotFound),
        "never through a symlink"
    );
    assert_eq!(
        service.update(ws(), "link.md", "", "pwned", 0).await,
        Err(MemoryError::NotFound)
    );
    assert_eq!(tokio::fs::read_to_string(&outside).await.unwrap(), "secret");
    let list = service.list(ws()).await.unwrap();
    assert!(
        list.items.iter().all(|i| i.file != "link.md"),
        "symlinks are not listed"
    );
}

#[tokio::test]
async fn oversized_files_are_refused() {
    let big = "x".repeat(MAX_FILE_BYTES as usize + 1);
    let (_root, service, _dir) = fixture(&[("big.md", &big), ("small.md", "y")]).await;
    assert_eq!(service.read(ws(), "big.md").await, Err(MemoryError::TooLarge));
    let small = service.read(ws(), "small.md").await.unwrap();
    assert_eq!(
        service.update(ws(), "small.md", "", &big, small.modified).await,
        Err(MemoryError::TooLarge)
    );
}

#[tokio::test]
async fn a_put_with_a_stale_mtime_is_a_conflict_and_a_fresh_one_writes_with_a_trash_copy() {
    let (_root, service, dir) = fixture(&[("rule.md", &memory("rule", "feedback", "Old."))]).await;
    let read = service.read(ws(), "rule.md").await.unwrap();
    assert_eq!(read.body, "Old.\n");
    let stale = service
        .update(ws(), "rule.md", &read.frontmatter_raw, "New.", read.modified - 1)
        .await;
    assert_eq!(
        stale,
        Err(MemoryError::Changed {
            current_modified: read.modified
        })
    );
    assert!(
        tokio::fs::read_to_string(dir.join("rule.md"))
            .await
            .unwrap()
            .contains("Old."),
        "a refused write leaves the file as it was"
    );

    let written = service
        .update(ws(), "rule.md", &read.frontmatter_raw, "New.", read.modified)
        .await
        .unwrap();
    assert_eq!(written.body, "New.\n");
    assert_eq!(written.frontmatter_raw, read.frontmatter_raw);
    let copies = trash_copies(&dir, "rule.md").await;
    assert_eq!(copies.len(), 1);
    let old = tokio::fs::read_to_string(dir.join(TRASH_DIR).join(&copies[0].1))
        .await
        .unwrap();
    assert!(old.contains("Old."), "the old content is in the trash");
}

#[tokio::test]
async fn delete_removes_the_index_line_keeps_a_trash_copy_and_restore_brings_both_back() {
    let index = "# Memory Index\n- [Rule](rule.md) — hook\n- [Other](other.md) — keep\n";
    let (_root, service, dir) = fixture(&[
        ("MEMORY.md", index),
        ("rule.md", &memory("rule", "feedback", "Keep tests green.")),
        ("other.md", &memory("other", "user", "Other.")),
    ])
    .await;
    let deleted = service.delete(ws(), "rule.md").await.unwrap();
    assert_eq!(deleted.index_lines_removed, 1);
    assert!(deleted.trash_file.starts_with(".trash/rule.md."));
    assert!(tokio::fs::symlink_metadata(dir.join("rule.md")).await.is_err());
    let now_index = tokio::fs::read_to_string(dir.join("MEMORY.md")).await.unwrap();
    assert_eq!(now_index, "# Memory Index\n- [Other](other.md) — keep\n");
    assert!(dir.join(&deleted.trash_file).exists());

    let restored = service.restore(ws(), "rule.md").await.unwrap();
    assert!(restored.body.contains("Keep tests green."));
    let index_after = tokio::fs::read_to_string(dir.join("MEMORY.md")).await.unwrap();
    assert!(index_after.contains("- [Rule](rule.md) — hook"), "{index_after}");
    assert!(index_after.contains("- [Other](other.md) — keep"));
    let list = service.list(ws()).await.unwrap();
    assert!(list.items.iter().any(|i| i.file == "rule.md" && i.in_index));

    assert_eq!(
        service.delete(ws(), "MEMORY.md").await,
        Err(MemoryError::IndexNotDeletable)
    );
    assert_eq!(
        service.restore(ws(), "never.md").await,
        Err(MemoryError::NothingToRestore)
    );
}

#[tokio::test]
async fn old_trash_copies_are_pruned() {
    let (_root, service, dir) = fixture(&[("rule.md", "x")]).await;
    let trash = dir.join(TRASH_DIR);
    tokio::fs::create_dir_all(&trash).await.unwrap();
    tokio::fs::write(trash.join("rule.md.1000"), "ancient").await.unwrap();
    service.delete(ws(), "rule.md").await.unwrap();
    assert!(!trash.join("rule.md.1000").exists(), "older than 30 days");
    assert_eq!(trash_copies(&dir, "rule.md").await.len(), 1);
}
