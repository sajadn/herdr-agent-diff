use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::diff::{DiffLine, parse_unified_diff};
use crate::model::{ChangeKind, CurrentFile, INLINE_TEXT_LIMIT, TextEligibility};
use crate::project::ProjectScope;

const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitComparison {
    WorkingTree,
    Unpushed,
    Commit { base: String, tip: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit {
    pub oid: String,
    pub parent: String,
    pub date: String,
    pub subject: String,
}

impl GitCommit {
    #[must_use]
    pub fn comparison(&self) -> GitComparison {
        GitComparison::Commit {
            base: self.parent.clone(),
            tip: self.oid.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitRange {
    pub newest: GitCommit,
    pub oldest: GitCommit,
    pub count: usize,
}

impl GitCommitRange {
    /// A combined diff includes every commit along one first-parent chain.
    pub fn from_commits(commits: &[&GitCommit]) -> Result<Self, String> {
        let newest = commits.first().ok_or("No commits selected.")?;
        let oldest = commits.last().ok_or("No commits selected.")?;
        if commits.windows(2).any(|pair| pair[0].parent != pair[1].oid) {
            return Err("Select consecutive commits on one first-parent chain; clear the filter if it hides commits.".into());
        }
        Ok(Self {
            newest: (*newest).clone(),
            oldest: (*oldest).clone(),
            count: commits.len(),
        })
    }

    #[must_use]
    pub fn comparison(&self) -> GitComparison {
        GitComparison::Commit {
            base: self.oldest.parent.clone(),
            tip: self.newest.oid.clone(),
        }
    }
}

/// Recent history reachable from HEAD, newest first. Merges use their first parent.
pub fn commits(root: &Path) -> Result<Vec<GitCommit>, String> {
    let output = run_git(
        root,
        [
            "log",
            "-z",
            "--max-count=200",
            "--date=short",
            "--format=%H%x00%P%x00%ad%x00%s",
            "HEAD",
            "--",
        ],
    )?;
    if !output.status.success() {
        return Err(command_error(&output, "Unable to read commit history"));
    }
    ensure_output_limit(&output.stdout, "Commit history")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<_> = text
        .strip_suffix('\0')
        .unwrap_or(&text)
        .split('\0')
        .collect();
    let mut commits = Vec::new();
    for record in fields.chunks_exact(4) {
        // `git log` hides parents at shallow boundaries. Read the actual object
        // before treating a commit as a root, so missing history is not shown as
        // a commit that added the entire repository.
        let parent = if let Some(parent) = record[1].split_whitespace().next() {
            Some(parent.to_owned())
        } else {
            let object = run_git(root, ["cat-file", "-p", record[0]])?;
            if !object.status.success() {
                return Err(command_error(&object, "Unable to read commit parents"));
            }
            String::from_utf8_lossy(&object.stdout)
                .lines()
                .take_while(|line| !line.is_empty())
                .find_map(|line| line.strip_prefix("parent ").map(str::to_owned))
        };
        let parent = if let Some(parent) = parent {
            parent
        } else {
            // Git supplies the empty tree for both SHA-1 and SHA-256 repositories.
            let empty = git_command(root)
                .args(["hash-object", "-t", "tree", "--stdin"])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|error| error.to_string())?;
            if !empty.status.success() {
                return Err(command_error(&empty, "Unable to resolve empty tree"));
            }
            String::from_utf8_lossy(&empty.stdout).trim().to_owned()
        };
        commits.push(GitCommit {
            oid: record[0].to_owned(),
            parent,
            date: record[2].to_owned(),
            subject: record[3].to_owned(),
        });
    }
    Ok(commits)
}

/// List committed files without checking out or walking the working directory.
pub fn revision_files(root: &Path, revision: &str) -> Result<Vec<CurrentFile>, String> {
    let output = run_git(root, ["ls-tree", "-r", "-l", "-z", revision, "--", "."])?;
    if !output.status.success() {
        return Err(command_error(&output, "Unable to list committed files"));
    }
    ensure_output_limit(&output.stdout, "Committed files")?;
    let scope = ProjectScope::discover(root);
    let mut files = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned an invalid tree record.".to_owned())?;
        let header = String::from_utf8_lossy(&record[..tab]);
        let fields: Vec<_> = header.split_whitespace().collect();
        // Do not follow committed symlinks or submodules.
        if fields.len() != 4 || !matches!(fields[0], "100644" | "100755") {
            continue;
        }
        let relative = path_from_bytes(&record[tab + 1..])?;
        if scope
            .as_ref()
            .is_some_and(|scope| !scope.contains_path(root, &relative))
        {
            continue;
        }
        let size = fields[3]
            .parse::<u64>()
            .map_err(|error| error.to_string())?;
        files.push(CurrentFile {
            absolute: root.join(&relative),
            relative,
            size,
            modified_unix_ns: None,
            text: if size > INLINE_TEXT_LIMIT {
                TextEligibility::Oversized
            } else {
                TextEligibility::Text
            },
        });
    }
    Ok(files)
}

pub fn revision_source(root: &Path, revision: &str, file: &CurrentFile) -> Result<String, String> {
    if file.size > INLINE_TEXT_LIMIT {
        return Err("File exceeds the 2 MiB preview limit.".into());
    }
    let mut object = OsString::from(format!("{revision}:./"));
    object.push(&file.relative);
    let output = git_command(root)
        .arg("show")
        .arg(object)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(command_error(&output, "Unable to read committed file"));
    }
    if output.stdout.len() as u64 > INLINE_TEXT_LIMIT {
        return Err("File exceeds the 2 MiB preview limit.".into());
    }
    if output.stdout.contains(&0) {
        return Err("Binary file; source preview unavailable.".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "File is not valid UTF-8.".into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitFileState {
    Staged,
    Unstaged,
    StagedAndUnstaged,
    Untracked,
    Committed,
}

impl GitFileState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::StagedAndUnstaged => "mixed",
            Self::Untracked => "untracked",
            Self::Committed => "committed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitChange {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub untracked: bool,
    pub comparison: GitComparison,
    pub state: GitFileState,
}

pub fn scan(root: &Path, comparison: &GitComparison) -> Result<Vec<GitChange>, String> {
    ensure_head(root)?;
    let scope = ProjectScope::discover(root);
    let reference = match comparison {
        GitComparison::WorkingTree => "HEAD".to_owned(),
        GitComparison::Unpushed => {
            let upstream = run_git(
                root,
                ["rev-parse", "--verify", "--abbrev-ref", "@{upstream}"],
            )?;
            if !upstream.status.success() {
                return Err(
                    "This branch has no tracked remote branch. Push it with `git push -u` to view unpushed commits."
                        .into(),
                );
            }
            "@{upstream}..HEAD".to_owned()
        }
        GitComparison::Commit { base, tip } => format!("{base}..{tip}"),
    };

    let tracked = run_git(
        root,
        [
            "diff",
            "--relative",
            "--name-status",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            &reference,
            "--",
        ],
    )?;
    if !tracked.status.success() {
        return Err(command_error(&tracked, "Git status failed"));
    }
    ensure_output_limit(&tracked.stdout, "Git status")?;

    let mut changes = parse_name_status(&tracked.stdout, comparison)?;
    if *comparison == GitComparison::WorkingTree {
        let statuses = scan_worktree_states(root)?;
        let untracked = run_git(
            root,
            ["ls-files", "--others", "--exclude-standard", "-z", "--"],
        )?;
        if !untracked.status.success() {
            return Err(command_error(&untracked, "Git untracked-file scan failed"));
        }
        ensure_output_limit(&untracked.stdout, "Git untracked-file scan")?;

        for change in &mut changes {
            change.state = change_state(&statuses, change);
        }

        let tracked_paths: BTreeSet<PathBuf> = changes
            .iter()
            .flat_map(|change| {
                change
                    .old_path
                    .iter()
                    .chain(std::iter::once(&change.path))
                    .cloned()
            })
            .collect();
        for path in untracked
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(path_from_bytes)
        {
            let path = path?;
            if !tracked_paths.contains(&path) {
                changes.push(GitChange {
                    kind: ChangeKind::Added,
                    path,
                    old_path: None,
                    untracked: true,
                    comparison: comparison.clone(),
                    state: GitFileState::Untracked,
                });
            }
        }
    }
    if let Some(scope) = scope {
        changes.retain(|change| {
            scope.contains_path(root, &change.path)
                || change
                    .old_path
                    .as_deref()
                    .is_some_and(|path| scope.contains_path(root, path))
        });
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

#[must_use]
pub fn unpushed_commit_count(root: &Path) -> Option<usize> {
    let upstream = run_git(
        root,
        ["rev-parse", "--verify", "--abbrev-ref", "@{upstream}"],
    )
    .ok()?;
    if !upstream.status.success() {
        return None;
    }
    let output = run_git(root, ["rev-list", "--count", "@{upstream}..HEAD"]).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

pub fn diff(root: &Path, change: &GitChange) -> Result<Vec<DiffLine>, String> {
    let output = if change.untracked {
        let mut command = git_command(root);
        command.args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--unified=3",
            "--",
        ]);
        command.arg("/dev/null").arg(root.join(&change.path));
        command
            .output()
            .map_err(|error| format!("Unable to run Git diff: {error}"))?
    } else {
        let reference = match &change.comparison {
            GitComparison::WorkingTree => "HEAD".to_owned(),
            GitComparison::Unpushed => "@{upstream}..HEAD".to_owned(),
            GitComparison::Commit { base, tip } => format!("{base}..{tip}"),
        };
        let mut command = git_command(root);
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            "--unified=3",
            &reference,
            "--",
        ]);
        command.arg(&change.path);
        if let Some(old_path) = &change.old_path {
            command.arg(old_path);
        }
        command
            .output()
            .map_err(|error| format!("Unable to run Git diff: {error}"))?
    };
    ensure_output_limit(&output.stdout, "Git diff")?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(command_error(&output, "Git diff failed"));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "Git diff contains non-UTF-8 output.".to_owned())?;
    Ok(parse_unified_diff(&text))
}

fn ensure_head(root: &Path) -> Result<(), String> {
    let head = run_git(root, ["rev-parse", "--verify", "HEAD"])?;
    if head.status.success() {
        Ok(())
    } else if String::from_utf8_lossy(&head.stderr).contains("not a git repository") {
        Err("This workspace is not inside a Git repository.".into())
    } else {
        Err("This Git repository has no HEAD commit yet.".into())
    }
}

fn parse_name_status(bytes: &[u8], comparison: &GitComparison) -> Result<Vec<GitChange>, String> {
    let mut tokens = bytes.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(status) = tokens.next().filter(|status| !status.is_empty()) {
        let status = String::from_utf8_lossy(status);
        let code = status.as_bytes().first().copied().unwrap_or(b'M') as char;
        let first = tokens
            .next()
            .ok_or_else(|| "Git returned an incomplete name-status record.".to_owned())
            .and_then(path_from_bytes)?;
        let (kind, path, old_path) = match code {
            'R' | 'C' => {
                let second = tokens
                    .next()
                    .ok_or_else(|| "Git returned an incomplete rename record.".to_owned())
                    .and_then(path_from_bytes)?;
                (
                    if code == 'R' {
                        ChangeKind::Renamed
                    } else {
                        ChangeKind::Added
                    },
                    second,
                    Some(first),
                )
            }
            'A' => (ChangeKind::Added, first, None),
            'D' => (ChangeKind::Deleted, first, None),
            _ => (ChangeKind::Modified, first, None),
        };
        changes.push(GitChange {
            kind,
            path,
            old_path,
            untracked: false,
            comparison: comparison.clone(),
            state: match comparison {
                GitComparison::WorkingTree => GitFileState::Unstaged,
                GitComparison::Unpushed | GitComparison::Commit { .. } => GitFileState::Committed,
            },
        });
    }
    Ok(changes)
}

fn scan_worktree_states(root: &Path) -> Result<BTreeMap<PathBuf, GitFileState>, String> {
    let output = run_git(
        root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ],
    )?;
    if !output.status.success() {
        return Err(command_error(&output, "Git status failed"));
    }
    ensure_output_limit(&output.stdout, "Git status")?;
    parse_porcelain_status(&output.stdout)
}

fn parse_porcelain_status(bytes: &[u8]) -> Result<BTreeMap<PathBuf, GitFileState>, String> {
    let mut tokens = bytes.split(|byte| *byte == 0);
    let mut statuses = BTreeMap::new();
    while let Some(record) = tokens.next().filter(|record| !record.is_empty()) {
        if record.len() < 3 {
            return Err("Git returned an incomplete status record.".to_owned());
        }
        let index = record[0] as char;
        let worktree = record[1] as char;
        let state = porcelain_state(index, worktree);
        let path = path_from_bytes(&record[3..])?;
        statuses.insert(path, state);

        if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            let old_path = tokens
                .next()
                .filter(|record| !record.is_empty())
                .ok_or_else(|| "Git returned an incomplete rename status record.".to_owned())
                .and_then(path_from_bytes)?;
            statuses.insert(old_path, state);
        }
    }
    Ok(statuses)
}

fn porcelain_state(index: char, worktree: char) -> GitFileState {
    if index == '?' && worktree == '?' {
        GitFileState::Untracked
    } else if index != ' ' && worktree != ' ' {
        GitFileState::StagedAndUnstaged
    } else if index != ' ' {
        GitFileState::Staged
    } else {
        GitFileState::Unstaged
    }
}

fn change_state(statuses: &BTreeMap<PathBuf, GitFileState>, change: &GitChange) -> GitFileState {
    statuses
        .get(&change.path)
        .or_else(|| change.old_path.as_ref().and_then(|path| statuses.get(path)))
        .copied()
        .unwrap_or(GitFileState::Unstaged)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "Git returned a non-UTF-8 path.".to_owned())
}

fn git_command(root: &Path) -> Command {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut safe_directory = OsString::from("safe.directory=");
    safe_directory.push(canonical_root.as_os_str());
    let mut command = Command::new("git");
    command
        .args(["-c"])
        .arg(safe_directory)
        .arg("-C")
        .arg(canonical_root)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run_git<const N: usize>(root: &Path, arguments: [&str; N]) -> Result<Output, String> {
    git_command(root)
        .args(arguments)
        .output()
        .map_err(|error| format!("Unable to run Git: {error}"))
}

fn ensure_output_limit(output: &[u8], operation: &str) -> Result<(), String> {
    if output.len() > MAX_GIT_OUTPUT_BYTES {
        Err(format!(
            "{operation} output exceeds the 32 MiB viewer limit."
        ))
    } else {
        Ok(())
    }
}

fn command_error(output: &Output, fallback: &str) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        fallback.to_owned()
    } else {
        format!("{fallback}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{GitComparison, GitFileState, diff, scan};
    use crate::diff::DiffLineKind;
    use crate::model::ChangeKind;

    #[test]
    fn scans_working_tree_and_renders_tracked_and_untracked_changes() {
        let directory = TempDir::new().expect("temp directory");
        git(directory.as_ref(), ["init", "-q"]);
        fs::write(directory.path().join("tracked.txt"), "before\n").expect("write tracked");
        fs::write(directory.path().join("staged.txt"), "before\n").expect("write staged");
        fs::write(directory.path().join("mixed.txt"), "before\n").expect("write mixed");
        git(
            directory.as_ref(),
            ["add", "tracked.txt", "staged.txt", "mixed.txt"],
        );
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        fs::write(directory.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked");
        fs::write(directory.path().join("staged.txt"), "before\nstaged\n").expect("modify staged");
        fs::write(directory.path().join("mixed.txt"), "before\nstaged\n").expect("modify mixed");
        git(directory.as_ref(), ["add", "mixed.txt"]);
        fs::write(
            directory.path().join("mixed.txt"),
            "before\nstaged\nunstaged\n",
        )
        .expect("modify mixed again");
        git(directory.as_ref(), ["add", "staged.txt"]);
        fs::write(directory.path().join("untracked.txt"), "new\n").expect("write untracked");

        let changes =
            scan(directory.path(), &GitComparison::WorkingTree).expect("scan git changes");
        assert_eq!(changes.len(), 4);
        let tracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("tracked.txt"))
            .expect("tracked change");
        assert_eq!(tracked.kind, ChangeKind::Modified);
        assert!(!tracked.untracked);
        assert_eq!(tracked.state, GitFileState::Unstaged);
        let tracked_lines = diff(directory.path(), tracked).expect("tracked diff");
        assert!(
            tracked_lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );

        let staged = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("staged.txt"))
            .expect("staged change");
        assert!(!staged.untracked);
        assert_eq!(staged.state, GitFileState::Staged);
        assert!(
            diff(directory.path(), staged)
                .expect("staged diff")
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );

        let mixed = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("mixed.txt"))
            .expect("mixed change");
        assert_eq!(mixed.state, GitFileState::StagedAndUnstaged);

        let untracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("untracked.txt"))
            .expect("untracked change");
        assert!(untracked.untracked);
        assert_eq!(untracked.state, GitFileState::Untracked);
        let untracked_lines = diff(directory.path(), untracked).expect("untracked diff");
        assert!(
            untracked_lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );
    }

    #[test]
    fn bazelproject_limits_working_tree_changes_to_selected_directories() {
        let directory = TempDir::new().expect("repository");
        git(directory.as_ref(), ["init", "-q"]);
        fs::create_dir_all(directory.path().join(".eclipse")).expect("eclipse directory");
        fs::create_dir_all(directory.path().join("java/selected")).expect("selected directory");
        fs::create_dir_all(directory.path().join("java/other")).expect("other directory");
        fs::write(
            directory.path().join(".eclipse/.bazelproject"),
            "directories:\n  java/selected\n",
        )
        .expect("project view");
        fs::write(directory.path().join("java/selected/Main.java"), "before\n")
            .expect("selected tracked");
        fs::write(directory.path().join("java/other/Other.java"), "before\n")
            .expect("other tracked");
        git(directory.as_ref(), ["add", "."]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        fs::write(
            directory.path().join("java/selected/Main.java"),
            "before\nafter\n",
        )
        .expect("selected modified");
        fs::write(
            directory.path().join("java/other/Other.java"),
            "before\nafter\n",
        )
        .expect("other modified");
        fs::write(
            directory.path().join("java/selected/Added.java"),
            "selected\n",
        )
        .expect("selected untracked");
        fs::write(directory.path().join("java/other/Added.java"), "other\n")
            .expect("other untracked");

        let changes = scan(directory.path(), &GitComparison::WorkingTree).expect("scan changes");
        let paths: Vec<_> = changes.iter().map(|change| &change.path).collect();
        assert_eq!(
            paths,
            [
                std::path::Path::new("java/selected/Added.java"),
                std::path::Path::new("java/selected/Main.java")
            ]
        );
    }

    #[test]
    fn ancestor_bazelproject_limits_changes_from_a_nested_git_workspace() {
        let repository = TempDir::new().expect("repository");
        git(repository.as_ref(), ["init", "-q"]);
        fs::write(
            repository.path().join(".bazelproject"),
            "directories:\n  project/java/selected\n",
        )
        .expect("project view");
        let project = repository.path().join("project");
        fs::create_dir_all(project.join("java/selected")).expect("selected directory");
        fs::create_dir_all(project.join("java/other")).expect("other directory");
        fs::write(project.join("java/selected/Main.java"), "before\n").expect("selected tracked");
        fs::write(project.join("java/other/Other.java"), "before\n").expect("other tracked");
        git(repository.as_ref(), ["add", "."]);
        git(
            repository.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        fs::write(project.join("java/selected/Main.java"), "before\nafter\n")
            .expect("selected modified");
        fs::write(project.join("java/other/Other.java"), "before\nafter\n")
            .expect("other modified");
        fs::write(project.join("java/selected/Added.java"), "selected\n")
            .expect("selected untracked");
        fs::write(project.join("java/other/Added.java"), "other\n").expect("other untracked");

        let changes = scan(&project, &GitComparison::WorkingTree).expect("scan changes");
        let paths: Vec<_> = changes.iter().map(|change| &change.path).collect();
        assert_eq!(
            paths,
            [
                std::path::Path::new("java/selected/Added.java"),
                std::path::Path::new("java/selected/Main.java")
            ]
        );
    }

    #[test]
    fn scans_committed_changes_that_are_not_pushed() {
        let directory = TempDir::new().expect("repository");
        let remote = TempDir::new().expect("remote");
        git(directory.as_ref(), ["init", "-q"]);
        git(directory.as_ref(), ["branch", "-M", "main"]);
        fs::write(directory.path().join("tracked.txt"), "before\n").expect("write tracked");
        git(directory.as_ref(), ["add", "tracked.txt"]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        git(remote.as_ref(), ["init", "--bare", "-q"]);
        git(
            directory.as_ref(),
            [
                "remote",
                "add",
                "origin",
                remote.path().to_str().expect("remote path"),
            ],
        );
        git(directory.as_ref(), ["push", "-q", "-u", "origin", "main"]);
        fs::write(directory.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked");
        git(directory.as_ref(), ["add", "tracked.txt"]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "local change",
            ],
        );

        let changes = scan(directory.path(), &GitComparison::Unpushed).expect("scan unpushed");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, std::path::Path::new("tracked.txt"));
        assert_eq!(changes[0].comparison, GitComparison::Unpushed);
        assert!(!changes[0].untracked);
        assert_eq!(changes[0].state, GitFileState::Committed);
        let lines = diff(directory.path(), &changes[0]).expect("unpushed diff");
        assert!(lines.iter().any(|line| line.kind == DiffLineKind::Addition));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scans_and_diffs_non_utf8_linux_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = TempDir::new().expect("repository");
        git(directory.as_ref(), ["init", "-q"]);
        fs::write(directory.path().join("tracked.txt"), "tracked\n").expect("write tracked");
        git(directory.as_ref(), ["add", "tracked.txt"]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        let invalid_name = OsString::from_vec(b"linux-\xff.txt".to_vec());
        fs::write(directory.path().join(&invalid_name), "new\n").expect("write non-UTF-8 path");

        let changes =
            scan(directory.path(), &GitComparison::WorkingTree).expect("scan git changes");
        let change = changes
            .iter()
            .find(|change| change.path.as_os_str() == invalid_name)
            .expect("non-UTF-8 change");
        assert!(change.untracked);
        assert!(
            diff(directory.path(), change)
                .expect("render non-UTF-8 diff")
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );
    }

    fn git<const N: usize>(directory: &std::path::Path, arguments: [&str; N]) {
        let status = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed: {status}");
    }
}
