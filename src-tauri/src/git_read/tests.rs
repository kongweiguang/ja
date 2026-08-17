// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::*;
use crate::workspace_read::WorkspaceRegistry;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    /// Creates a path with spaces so Git's NUL status mode is exercised with
    /// the same names users choose in a desktop workspace.
    fn create() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ja git fixture {suffix}"));
        fs::create_dir_all(&path).expect("create git fixture");
        Self(path)
    }
}

impl Drop for TempDir {
    /// Removes only this uniquely named test repository after child cleanup.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Finds the same native Git executable admission uses without assuming a
/// fixed installation path on the test host.
fn git_program() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(if cfg!(windows) { "git.exe" } else { "git" });
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }
    None
}

/// Builds a real temporary repository using only fixture setup commands.
fn run_git(git: &PathBuf, root: &PathBuf, args: &[&str]) {
    let status = Command::new(git)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .args(args)
        .status()
        .expect("run fixture git");
    assert!(status.success(), "fixture git failed: {args:?}");
}

/// Creates a tracked Unicode/space fixture so machine-format parsing is real.
fn fixture_repo() -> Option<(TempDir, PathBuf)> {
    let git = git_program()?;
    let root = TempDir::create();
    run_git(&git, &root.0, &["init", "--initial-branch=main", "-q"]);
    fs::write(root.0.join("文件 name.txt"), "initial\n").expect("write tracked fixture");
    run_git(&git, &root.0, &["add", "--", "文件 name.txt"]);
    run_git(
        &git,
        &root.0,
        &[
            "-c",
            "user.name=JA Test",
            "-c",
            "user.email=ja@example.invalid",
            "commit",
            "-m",
            "fixture",
            "--no-verify",
        ],
    );
    Some((root, git))
}

/// Proves all exposed Git operations use machine-safe output and do not write.
#[test]
fn typed_git_read_operations_are_machine_safe_and_read_only() {
    let Some((root, git)) = fixture_repo() else {
        return;
    };
    run_git(
        &git,
        &root.0,
        &["config", "alias.status", "!echo MALICIOUS"],
    );
    run_git(&git, &root.0, &["config", "core.pager", "!echo MALICIOUS"]);
    fs::write(root.0.join("文件 name.txt"), "changed\n").expect("edit tracked fixture");
    fs::write(root.0.join("untracked space.txt"), "untracked\n").expect("write untracked fixture");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register git root");
    let workspace = registry.get(info.id).expect("get git root");
    let adapter = GitReadOnly::new(workspace).expect("git adapter");
    assert!(adapter.replace_refs_are_disabled_for_test());
    assert!(adapter.object_fetch_environment_is_hardened_for_test());
    let token = CancellationToken::default();
    let status = adapter.status(&token).expect("status");
    assert!(status.iter().any(|entry| entry.path == "文件 name.txt"));
    assert!(
        status
            .iter()
            .any(|entry| entry.path == "untracked space.txt")
    );
    let diff = adapter.diff(&DiffOptions::default(), &token).expect("diff");
    assert!(String::from_utf8_lossy(&diff.bytes).contains("changed"));
    let log = adapter.log(10, &token).expect("log");
    assert_eq!(log.len(), 1);
    assert!(log[0].parents.is_empty());
    assert_eq!(log[0].author, "JA Test");
    assert_eq!(log[0].subject, "fixture");
    let show = adapter
        .show("HEAD", Some("文件 name.txt"), &token)
        .expect("show");
    assert!(String::from_utf8_lossy(&show.bytes).contains("initial"));
}

/// Proves the typed surface rejects traversal, option injection and cancellation.
#[test]
fn invalid_paths_objects_and_cancellation_are_rejected() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register git root");
    let adapter =
        GitReadOnly::new(registry.get(info.id).expect("get git root")).expect("git adapter");
    let token = CancellationToken::default();
    assert!(matches!(
        adapter.diff(
            &DiffOptions {
                staged: false,
                relative_path: Some("../escape".to_owned()),
            },
            &token,
        ),
        Err(GitError::InvalidPath)
    ));
    assert!(matches!(
        adapter.diff(
            &DiffOptions {
                staged: false,
                relative_path: Some("foo\\bar".to_owned()),
            },
            &token,
        ),
        Err(GitError::InvalidPath)
    ));
    #[cfg(windows)]
    assert!(matches!(
        adapter.diff(
            &DiffOptions {
                staged: false,
                relative_path: Some("NUL".to_owned()),
            },
            &token,
        ),
        Err(GitError::InvalidPath)
    ));
    assert!(matches!(
        adapter.show("-c", None, &token),
        Err(GitError::InvalidObject)
    ));
    token.cancel();
    assert!(matches!(adapter.status(&token), Err(GitError::Cancelled)));
}

/// Proves invalid UTF-8 is rejected instead of being lossy-replaced into a
/// different filename that the workbench could later request.
#[test]
fn parser_rejects_non_utf8_path_records() {
    assert!(matches!(
        super::parse::parse_status(&[b'?', b' ', 0xff, 0], 10),
        Err(GitError::Parse)
    ));
}

/// Proves every porcelain record consumes the shared budget and a rename's
/// NUL-delimited original path cannot bypass the status record limit.
#[test]
fn status_parser_counts_all_record_types_and_rename_companion() {
    let bytes = b"# branch.head main\0\
1 XY N... 100644 100644 100644 abcdef abcdef file.txt\0\
2 R. N... 100644 100644 100644 abcdef abcdef R100 renamed.txt\0old.txt\0\
u UU N... 100644 100644 100644 100644 abcdef abcdef abcdef conflict.txt\0\
? untracked\0! ignored\0";
    assert_eq!(
        super::parse::parse_status(bytes, 7)
            .expect("exact status record budget")
            .len(),
        6
    );
    assert!(matches!(
        super::parse::parse_status(bytes, 6),
        Err(GitError::Parse)
    ));
}

/// Proves short NUL records fail before a partial result can escape when the
/// parser is just over its explicit record budget.
#[test]
fn status_parser_rejects_record_overflow_without_partial_result() {
    let max_records = 4096;
    let mut bytes = Vec::with_capacity((max_records + 1) * 4);
    for _ in 0..=max_records {
        bytes.extend_from_slice(b"? x\0");
    }
    assert!(matches!(
        super::parse::parse_status(&bytes, max_records),
        Err(GitError::Parse)
    ));
    bytes.truncate(bytes.len() - 4);
    assert_eq!(
        super::parse::parse_status(&bytes, max_records)
            .expect("near-limit status records")
            .len(),
        max_records
    );
}

/// Proves a linked worktree cannot make an external Git directory trusted.
#[test]
fn external_gitdir_pointer_is_rejected() {
    let root = TempDir::create();
    let outside = TempDir::create();
    fs::write(
        root.0.join(".git"),
        format!("gitdir: {}\n", outside.0.display()),
    )
    .expect("write external gitdir pointer");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register root");
    let workspace = registry.get(info.id).expect("get root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves both alternates transports are refused even when the value points
/// back into the workspace, so `show HEAD` cannot reach an external object.
#[test]
fn alternates_files_are_rejected_before_object_reads() {
    for file_name in ["alternates", "http-alternates"] {
        for value in ["../objects\n", "external-object-store\n"] {
            let Some((root, _git)) = fixture_repo() else {
                return;
            };
            let path = root
                .0
                .join(".git")
                .join("objects")
                .join("info")
                .join(file_name);
            fs::write(path, value).expect("write alternates fixture");
            let registry = WorkspaceRegistry::default();
            let info = registry
                .register(&root.0)
                .expect("register alternates root");
            let workspace = registry.get(info.id).expect("get alternates root");
            assert!(matches!(
                GitReadOnly::new(workspace),
                Err(GitError::ExternalWorktree)
            ));
        }
    }
}

/// Proves an empty alternates file is the only accepted alternates state and
/// malformed UTF-8 is rejected without a lossy path interpretation.
#[test]
fn alternates_empty_and_invalid_bytes_are_bounded() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let path = root
        .0
        .join(".git")
        .join("objects")
        .join("info")
        .join("alternates");
    fs::write(&path, []).expect("write empty alternates");
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register empty alternates root");
    let workspace = registry.get(info.id).expect("get empty alternates root");
    assert!(GitReadOnly::new(workspace.clone()).is_ok());
    fs::write(path, [0xff]).expect("write invalid alternates");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves an alternates file cannot consume an unbounded metadata buffer or
/// alias a file outside the workspace through a hard link.
#[test]
fn alternates_size_and_hard_link_are_rejected() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let path = root
        .0
        .join(".git")
        .join("objects")
        .join("info")
        .join("alternates");
    fs::write(&path, vec![b'a'; 4 * 1024 + 1]).expect("write oversized alternates");
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register oversized alternates root");
    let workspace = registry
        .get(info.id)
        .expect("get oversized alternates root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));

    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let outside = TempDir::create();
    let outside_file = outside.0.join("objects.txt");
    fs::write(&outside_file, "external\n").expect("write external alternates target");
    let path = root
        .0
        .join(".git")
        .join("objects")
        .join("info")
        .join("alternates");
    if fs::hard_link(&outside_file, &path).is_err() {
        return;
    }
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register hard-linked alternates root");
    let workspace = registry
        .get(info.id)
        .expect("get hard-linked alternates root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves local config cannot include another file, enable lazy objects or
/// invoke external processes/network paths through Git configuration.
#[test]
fn unsafe_local_git_config_is_rejected_without_following_paths() {
    let outside = TempDir::create();
    let outside_path = outside.0.display().to_string();
    let cases = [
        format!("[include]\n\tpath = {outside_path}\n"),
        format!("[includeIf \"gitdir:ja\"]\n\tpath = {outside_path}\n"),
        "[extensions]\n\tworktreeConfig = true\n".to_owned(),
        "[extensions]\n\tpartialClone = origin\n".to_owned(),
        "[remote \"origin\"]\n\tpromisor = true\n".to_owned(),
        format!("[core]\n\tworktree = {outside_path}\n"),
        "[core]\n\tfsmonitor = true\n".to_owned(),
        "[core]\n\tsshCommand = ssh external\n".to_owned(),
        "[diff]\n\texternal = !echo external\n".to_owned(),
    ];
    for config in cases {
        let Some((root, _git)) = fixture_repo() else {
            return;
        };
        fs::write(root.0.join(".git").join("config"), config).expect("write unsafe config");
        let registry = WorkspaceRegistry::default();
        let info = registry
            .register(&root.0)
            .expect("register unsafe config root");
        let workspace = registry.get(info.id).expect("get unsafe config root");
        assert!(matches!(
            GitReadOnly::new(workspace),
            Err(GitError::ExternalWorktree)
        ));
    }
}

/// Proves the worktree-specific config is checked with the same bounded parser
/// before an internal Git command can honor an external working-tree path.
#[test]
fn unsafe_worktree_config_is_rejected_without_following_paths() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let outside = TempDir::create();
    fs::write(
        root.0.join(".git").join("config.worktree"),
        format!("[core]\n\tworktree = {}\n", outside.0.display()),
    )
    .expect("write unsafe worktree config");
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register unsafe worktree config root");
    let workspace = registry
        .get(info.id)
        .expect("get unsafe worktree config root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves a promisor pack marker and submodule metadata are unsupported rather
/// than allowing Git to consult another object or repository trust root.
#[test]
fn promisor_and_submodule_metadata_are_rejected() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    fs::write(
        root.0
            .join(".git")
            .join("objects")
            .join("pack")
            .join("pack.promisor"),
        [],
    )
    .expect("write promisor marker");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register promisor root");
    let workspace = registry.get(info.id).expect("get promisor root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));

    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    fs::write(root.0.join(".gitmodules"), "[submodule \"external\"]\n")
        .expect("write submodule metadata");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register submodule root");
    let workspace = registry.get(info.id).expect("get submodule root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves a normal repository commondir pointer cannot redirect object reads
/// outside the root even when `.git` itself remains local.
#[test]
fn external_commondir_pointer_is_rejected() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let outside = TempDir::create();
    fs::write(
        root.0.join(".git").join("commondir"),
        format!("{}\n", outside.0.display()),
    )
    .expect("write external commondir");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register commondir root");
    let workspace = registry.get(info.id).expect("get commondir root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves a bare repository cannot bypass object-store validation by placing
/// its HEAD and alternates directly at the admitted workspace root.
#[test]
fn bare_repository_alternates_are_rejected() {
    let Some(git) = git_program() else {
        return;
    };
    let root = TempDir::create();
    run_git(&git, &root.0, &["init", "--bare", "-q"]);
    fs::write(
        root.0.join("objects").join("info").join("alternates"),
        "external-object-store\n",
    )
    .expect("write bare alternates fixture");
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register bare root");
    let workspace = registry.get(info.id).expect("get bare root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves an external secret hard-linked into every Git object metadata class
/// is rejected before a read-only adapter can expose it through `show`.
#[test]
fn external_object_hard_links_are_rejected_across_store_entries() {
    let targets = [
        ".git/objects/aa/0123456789abcdef0123456789abcdef01234567",
        ".git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack",
        ".git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.idx",
        ".git/objects/pack/multi-pack-index",
        ".git/objects/info/packs",
        ".git/objects/info/commit-graph",
        ".git/objects/info/commit-graphs/graph-0123456789abcdef0123456789abcdef01234567.graph",
        ".git/objects/info/commit-graphs/commit-graph-chain",
    ];
    for target in targets {
        let Some((root, _git)) = fixture_repo() else {
            return;
        };
        let outside = TempDir::create();
        let secret = outside.0.join("EXTERNAL-SECRET");
        fs::write(&secret, b"EXTERNAL-SECRET").expect("write external object secret");
        let object_path = root
            .0
            .join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(object_path.parent().expect("object parent"))
            .expect("create object metadata parent");
        fs::hard_link(&secret, &object_path).expect("create external object hard link");
        let registry = WorkspaceRegistry::default();
        let info = registry
            .register(&root.0)
            .expect("register hard-linked object root");
        let workspace = registry.get(info.id).expect("get hard-linked object root");
        assert!(matches!(
            GitReadOnly::new(workspace),
            Err(GitError::ExternalWorktree)
        ));
    }
}

/// Proves a fanout directory replacement with a symlink/reparse point fails
/// closed before recursive object traversal can cross its target.
#[test]
fn fanout_directory_link_is_rejected() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let outside = TempDir::create();
    let fanout = root.0.join(".git").join("objects").join("cc");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&outside.0, &fanout).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&outside.0, &fanout).is_ok();
    #[cfg(not(any(unix, windows)))]
    let linked = false;
    if !linked {
        return;
    }
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register linked fanout root");
    let workspace = registry.get(info.id).expect("get linked fanout root");
    assert!(matches!(
        GitReadOnly::new(workspace),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves object directory count, file count, depth and absolute deadline are
/// enforced without constructing a huge repository fixture.
#[test]
fn object_store_scan_limits_fail_closed() {
    let Some((root, _git)) = fixture_repo() else {
        return;
    };
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register budget root");
    let workspace = registry.get(info.id).expect("get budget root");
    let normal = (
        1,
        200_000,
        4 * 1024 * 1024 * 1024,
        4,
        Duration::from_secs(2),
    );
    assert!(matches!(
        super::command::validate_object_directory_with_test_limits(
            &workspace,
            ".git/objects",
            normal.0,
            normal.1,
            normal.2,
            normal.3,
            normal.4,
        ),
        Err(GitError::ExternalWorktree)
    ));
    let file_overflow = (
        8 * 1024,
        0,
        4 * 1024 * 1024 * 1024,
        4,
        Duration::from_secs(2),
    );
    assert!(matches!(
        super::command::validate_object_directory_with_test_limits(
            &workspace,
            ".git/objects",
            file_overflow.0,
            file_overflow.1,
            file_overflow.2,
            file_overflow.3,
            file_overflow.4,
        ),
        Err(GitError::ExternalWorktree)
    ));
    let byte_overflow = (8 * 1024, 200_000, 0, 4, Duration::from_secs(2));
    assert!(matches!(
        super::command::validate_object_directory_with_test_limits(
            &workspace,
            ".git/objects",
            byte_overflow.0,
            byte_overflow.1,
            byte_overflow.2,
            byte_overflow.3,
            byte_overflow.4,
        ),
        Err(GitError::ExternalWorktree)
    ));
    let deadline = (8 * 1024, 200_000, 4 * 1024 * 1024 * 1024, 4, Duration::ZERO);
    assert!(matches!(
        super::command::validate_object_directory_with_test_limits(
            &workspace,
            ".git/objects",
            deadline.0,
            deadline.1,
            deadline.2,
            deadline.3,
            deadline.4,
        ),
        Err(GitError::ExternalWorktree)
    ));
}

/// Proves the strict grammar still admits an ordinary repository after Git
/// compacts loose objects into pack/index metadata.
#[test]
fn packed_repository_remains_readable() {
    let Some((root, git)) = fixture_repo() else {
        return;
    };
    run_git(&git, &root.0, &["gc", "--aggressive", "--no-quiet"]);
    run_git(&git, &root.0, &["update-server-info"]);
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register packed root");
    let workspace = registry.get(info.id).expect("get packed root");
    let adapter = GitReadOnly::new(workspace).expect("packed git adapter");
    let token = CancellationToken::default();
    assert_eq!(adapter.log(10, &token).expect("packed log").len(), 1);
    assert!(
        String::from_utf8_lossy(
            &adapter
                .show("HEAD", None, &token)
                .expect("packed show")
                .bytes,
        )
        .contains("fixture")
    );
}

/// Proves split commit-graph metadata stays inside the strict info-tree
/// grammar while the normal Git read surface remains usable.
#[test]
fn split_commit_graph_repository_remains_readable() {
    let Some((root, git)) = fixture_repo() else {
        return;
    };
    run_git(
        &git,
        &root.0,
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let registry = WorkspaceRegistry::default();
    let info = registry
        .register(&root.0)
        .expect("register split commit graph root");
    let workspace = registry.get(info.id).expect("get split commit graph root");
    let adapter = GitReadOnly::new(workspace).expect("split commit graph adapter");
    assert_eq!(
        adapter
            .log(10, &CancellationToken::default())
            .expect("split commit graph log")
            .len(),
        1
    );
}
