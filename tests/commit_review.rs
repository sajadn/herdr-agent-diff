use std::fs;
use std::path::Path;
use std::process::Command;

use herdr_agent_diff::git::{GitCommitRange, commits, diff, revision_files, revision_source, scan};
use herdr_agent_diff::model::ChangeKind;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn repository(format: &str) -> TempDir {
    let directory = TempDir::new().unwrap();
    git(directory.path(), &["init", "-q", "-b", "main", format]);
    git(directory.path(), &["config", "user.name", "Test"]);
    git(
        directory.path(),
        &["config", "user.email", "test@example.com"],
    );
    directory
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "--all"]);
    git(root, &["commit", "-qm", message]);
}

#[test]
fn historical_diff_and_files_ignore_worktree_and_leave_repository_unchanged() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::write(root.join("code.rs"), "original\n").unwrap();
    commit(root, "initial");
    fs::write(root.join("code.rs"), "selected version\n").unwrap();
    commit(root, "selected commit");
    fs::write(root.join("code.rs"), "latest version\n").unwrap();
    commit(root, "latest commit");
    fs::write(root.join("code.rs"), "uncommitted version\n").unwrap();
    fs::write(root.join("untracked.rs"), "local only\n").unwrap();
    let before = git(root, &["status", "--porcelain=v1"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    let history = commits(root).unwrap();
    assert_eq!(
        history
            .iter()
            .map(|c| c.subject.as_str())
            .collect::<Vec<_>>(),
        ["latest commit", "selected commit", "initial"]
    );
    let selected = &history[1];
    let changes = scan(root, &selected.comparison()).unwrap();
    assert_eq!(changes.len(), 1);
    let patch = diff(root, &changes[0]).unwrap();
    assert!(
        patch
            .iter()
            .any(|line| line.text.contains("selected version"))
    );
    assert!(
        !patch
            .iter()
            .any(|line| line.text.contains("latest") || line.text.contains("uncommitted"))
    );
    let files = revision_files(root, &selected.oid).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        revision_source(root, &selected.oid, &files[0]).unwrap(),
        "selected version\n"
    );
    assert_eq!(git(root, &["status", "--porcelain=v1"]), before);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
}

#[test]
fn selected_range_shows_net_changes_and_newest_file_contents() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::write(root.join("code.rs"), "before\n").unwrap();
    commit(root, "base");
    fs::write(root.join("code.rs"), "intermediate\n").unwrap();
    fs::write(root.join("temporary.txt"), "temporary\n").unwrap();
    commit(root, "first selected");
    fs::write(root.join("code.rs"), "after\n").unwrap();
    fs::remove_file(root.join("temporary.txt")).unwrap();
    commit(root, "last selected");
    fs::write(root.join("code.rs"), "working copy\n").unwrap();
    let before = git(root, &["status", "--porcelain=v1"]);
    let history = commits(root).unwrap();
    let range = GitCommitRange::from_commits(&[&history[0], &history[1]]).unwrap();
    let changes = scan(root, &range.comparison()).unwrap();
    assert_eq!(range.count, 2);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, Path::new("code.rs"));
    let patch = diff(root, &changes[0]).unwrap();
    assert!(patch.iter().any(|line| line.text == "-before"));
    assert!(patch.iter().any(|line| line.text == "+after"));
    assert!(
        !patch
            .iter()
            .any(|line| line.text.contains("intermediate") || line.text.contains("working copy"))
    );
    let files = revision_files(root, &range.newest.oid).unwrap();
    assert_eq!(
        revision_source(root, &range.newest.oid, &files[0]).unwrap(),
        "after\n"
    );
    assert_eq!(git(root, &["status", "--porcelain=v1"]), before);
    // Including a true root commit also works as a range.
    let range = GitCommitRange::from_commits(&history.iter().collect::<Vec<_>>()).unwrap();
    let changes = scan(root, &range.comparison()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Added);
    assert!(GitCommitRange::from_commits(&[&history[0], &history[2]]).is_err());
}

#[test]
fn root_commits_work_with_sha1_and_sha256() {
    for format in ["--object-format=sha1", "--object-format=sha256"] {
        let directory = repository(format);
        let root = directory.path();
        fs::write(root.join("first.txt"), "first\n").unwrap();
        commit(root, "root");
        let history = commits(root).unwrap();
        let changes = scan(root, &history[0].comparison()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Added);
        assert!(
            diff(root, &changes[0])
                .unwrap()
                .iter()
                .any(|line| line.text.contains("first"))
        );
    }
}

#[test]
fn shallow_boundary_is_not_mistaken_for_a_root_commit() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::write(root.join("file.txt"), "before\n").unwrap();
    commit(root, "initial");
    let parent = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("file.txt"), "after\n").unwrap();
    commit(root, "update");
    let shallow = TempDir::new().unwrap();
    git(
        shallow.path(),
        &[
            "clone",
            "-q",
            "--depth=1",
            &format!("file://{}", root.display()),
            ".",
        ],
    );
    let selected = commits(shallow.path()).unwrap().remove(0);
    assert_eq!(selected.parent, parent);
    assert!(scan(shallow.path(), &selected.comparison()).is_err());
}

#[test]
fn renames_deletions_and_literal_paths_are_preserved() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::write(root.join("old[1].txt"), "same content\n").unwrap();
    fs::write(root.join("delete.txt"), "deleted content\n").unwrap();
    fs::write(root.join("new1.txt"), "unrelated before\n").unwrap();
    commit(root, "initial");
    git(root, &["mv", "old[1].txt", "new[1].txt"]);
    fs::remove_file(root.join("delete.txt")).unwrap();
    fs::write(root.join("new1.txt"), "unrelated after\n").unwrap();
    commit(root, "rename and delete");
    let selected = commits(root).unwrap().remove(0);
    let changes = scan(root, &selected.comparison()).unwrap();
    let renamed = changes
        .iter()
        .find(|change| change.kind == ChangeKind::Renamed)
        .unwrap();
    assert_eq!(renamed.old_path.as_deref(), Some(Path::new("old[1].txt")));
    let patch = diff(root, renamed).unwrap();
    assert!(!patch.iter().any(|line| line.text.contains("unrelated")));
    let deleted = changes
        .iter()
        .find(|change| change.kind == ChangeKind::Deleted)
        .unwrap();
    assert!(
        diff(root, deleted)
            .unwrap()
            .iter()
            .any(|line| line.text.contains("deleted content"))
    );
    let files = revision_files(root, &selected.oid).unwrap();
    assert!(
        !files
            .iter()
            .any(|file| file.relative == Path::new("delete.txt"))
    );
}

#[test]
fn merge_commits_compare_with_first_parent() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::write(root.join("base.txt"), "base\n").unwrap();
    commit(root, "base");
    git(root, &["checkout", "-qb", "feature"]);
    fs::write(root.join("feature.txt"), "feature\n").unwrap();
    commit(root, "feature");
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("main.txt"), "main\n").unwrap();
    commit(root, "main change");
    let first_parent = git(root, &["rev-parse", "HEAD"]);
    git(
        root,
        &["merge", "--no-ff", "-qm", "merge feature", "feature"],
    );
    let selected = commits(root).unwrap().remove(0);
    assert_eq!(selected.parent, first_parent);
    let changes = scan(root, &selected.comparison()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, Path::new("feature.txt"));
}

#[test]
fn historical_files_respect_subdirectory_and_reject_binary_and_large_previews() {
    let directory = repository("--object-format=sha1");
    let root = directory.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("outside.txt"), "outside\n").unwrap();
    fs::write(root.join("sub/code.rs"), "historical\n").unwrap();
    fs::write(root.join("sub/binary"), b"binary\0data").unwrap();
    fs::write(root.join("sub/large"), vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    commit(root, "files");
    let sub = root.join("sub");
    let selected = commits(&sub).unwrap().remove(0);
    let files = revision_files(&sub, &selected.oid).unwrap();
    assert_eq!(files.len(), 3);
    for file in files {
        let source = revision_source(&sub, &selected.oid, &file);
        if file.relative == Path::new("code.rs") {
            assert_eq!(source.unwrap(), "historical\n");
        } else {
            assert!(source.is_err());
        }
    }
}
