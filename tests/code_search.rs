use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;

use herdr_agent_diff::{
    git::revision_files, model::INLINE_TEXT_LIMIT, search::search_files, snapshot::scan,
};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn searches_working_files_with_ignore_rules_and_read_limits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("source.txt"), "first\nα needle β\nNeedle\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "needle\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "needle\n").unwrap();
    std::fs::write(root.join("binary"), b"needle\0").unwrap();
    let large = std::fs::File::create(root.join("large")).unwrap();
    large.set_len(INLINE_TEXT_LIMIT + 1).unwrap();
    let (files, _) = scan(root).unwrap();
    let files: Vec<_> = files.into_values().collect();
    let result = search_files(root, &files, None, "needle", &AtomicU64::new(1), 1).unwrap();
    assert_eq!(result.hits.len(), 2);
    let hit = result
        .hits
        .iter()
        .find(|hit| hit.path == Path::new("source.txt"))
        .unwrap();
    assert_eq!(hit.line, 1);
    assert!(hit.preview.contains("α needle β"));
    assert!(result.skipped >= 2);
    assert!(!result.limited);
}

#[test]
fn searches_selected_revision_without_reading_uncommitted_contents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init"]);
    std::fs::write(root.join("source.txt"), "committed needle\n").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    let head = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("source.txt"), "local change\n").unwrap();
    let status = git(root, &["status", "--porcelain=v1"]);
    let files = revision_files(root, &head).unwrap();
    let result = search_files(root, &files, Some(&head), "needle", &AtomicU64::new(1), 1).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].preview, "committed needle");
    assert_eq!(git(root, &["status", "--porcelain=v1"]), status);
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).unwrap(),
        "local change\n"
    );
}

#[test]
fn cancels_stale_requests_and_reports_truncated_results() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("many.txt"), "needle\n".repeat(1_001)).unwrap();
    let (files, _) = scan(temp.path()).unwrap();
    let files: Vec<_> = files.into_values().collect();
    assert!(search_files(temp.path(), &files, None, "needle", &AtomicU64::new(2), 1).is_none());
    let result = search_files(temp.path(), &files, None, "needle", &AtomicU64::new(1), 1).unwrap();
    assert_eq!(result.hits.len(), 1_000);
    assert!(result.limited);
    let empty = search_files(temp.path(), &files, None, "", &AtomicU64::new(1), 1).unwrap();
    assert!(empty.hits.is_empty());
}

#[cfg(unix)]
#[test]
fn refuses_a_file_replaced_by_a_symlink_after_scanning() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.txt"), "initial\n").unwrap();
    std::fs::write(temp.path().join("outside.txt"), "needle\n").unwrap();
    let (files, _) = scan(&root).unwrap();
    let files: Vec<_> = files.into_values().collect();
    std::fs::remove_file(root.join("source.txt")).unwrap();
    std::os::unix::fs::symlink(temp.path().join("outside.txt"), root.join("source.txt")).unwrap();
    let result = search_files(&root, &files, None, "needle", &AtomicU64::new(1), 1).unwrap();
    assert!(result.hits.is_empty());
    assert_eq!(result.skipped, 1);
}
