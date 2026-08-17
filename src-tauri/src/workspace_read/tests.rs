// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    /// Uses a unique Unicode/space-bearing path so path serialization is tested
    /// at the same boundary as ordinary temporary fixtures.
    fn create() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ja workspace 测试 {} {suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("create fixture root");
        Self(path)
    }
}

impl Drop for TempDir {
    /// Test cleanup is intentionally best effort because the assertion already
    /// proves the production reader never owns the fixture directory.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Registers a fixture through the same opaque-id path as production callers.
fn workspace(root: &TempDir) -> WorkspaceHandle {
    let registry = WorkspaceRegistry::default();
    let info = registry.register(&root.0).expect("register fixture root");
    registry.get(info.id).expect("lookup workspace")
}

/// Proves page cursors and containment reject traversal while preserving order.
#[test]
fn registry_tree_pages_and_containment_are_bounded() {
    let root = TempDir::create();
    fs::write(root.0.join("b space.txt"), "b").expect("write b");
    fs::write(root.0.join("a.txt"), "a").expect("write a");
    fs::create_dir(root.0.join("src")).expect("create src");
    fs::write(root.0.join("src").join("main.rs"), "fn main() {}").expect("write main");
    let handle = workspace(&root);
    assert!(matches!(
        handle.resolve_file("../escape"),
        Err(WorkspaceError::InvalidRelativePath)
    ));
    assert!(matches!(
        handle.resolve_file("./a.txt"),
        Err(WorkspaceError::InvalidRelativePath)
    ));
    assert!(matches!(
        handle.resolve_file("a:b"),
        Err(WorkspaceError::InvalidRelativePath)
    ));
    assert!(matches!(
        handle.resolve_file(""),
        Err(WorkspaceError::NotFile)
    ));
    assert_eq!(
        handle.metadata("", 1024).expect("root metadata").kind,
        EntryKind::Directory
    );

    let reader = TreeReader::new(
        handle,
        TreePolicy {
            max_page_size: 1,
            ..TreePolicy::default()
        },
    );
    let first = reader
        .read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: None,
            page_size: Some(1),
            snapshot_token: None,
        })
        .expect("first page");
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    let second = reader
        .read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: first.next_cursor.clone(),
            page_size: Some(1),
            snapshot_token: Some(first.snapshot_token.clone()),
        })
        .expect("second page");
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].name, "b space.txt");

    fs::write(root.0.join("b space.txt"), "changed").expect("change cursor fixture");
    assert!(matches!(
        reader.read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: second.next_cursor.clone(),
            page_size: Some(1),
            snapshot_token: Some(second.snapshot_token.clone()),
        }),
        Err(WorkspaceError::StaleCursor)
    ));

    let bounded = TreeReader::new(
        workspace(&root),
        TreePolicy {
            max_entries_per_page_scan: 2,
            ..TreePolicy::default()
        },
    );
    assert!(matches!(
        bounded.read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: None,
            page_size: None,
            snapshot_token: None,
        }),
        Err(WorkspaceError::EntryBudgetExceeded)
    ));
}

/// Proves the reader never guesses an encoding or returns oversized content.
#[test]
fn content_reader_classifies_text_binary_encoding_and_size() {
    let root = TempDir::create();
    fs::write(root.0.join("utf8.txt"), "hello 世界").expect("write utf8");
    fs::write(root.0.join("bom.txt"), [0xef, 0xbb, 0xbf, b'h', b'i']).expect("write bom");
    fs::write(root.0.join("utf16.txt"), [0xff, 0xfe, b'h', 0, b'i', 0]).expect("write utf16");
    fs::write(root.0.join("binary.bin"), [0, 1, 2, 3]).expect("write binary");
    fs::write(root.0.join("large.bin"), [1, 2, 3, 4, 5, 6, 7, 8, 9]).expect("write large");
    let reader = FileReader::new(
        workspace(&root),
        ContentPolicy {
            max_bytes: 8,
            hash_limit_bytes: 8,
        },
    );
    assert_eq!(
        reader.read("utf8.txt").expect("utf8").kind,
        ContentKind::TooLarge
    );
    let bom = reader.read("bom.txt").expect("bom");
    assert_eq!(bom.encoding, Some(TextEncoding::Utf8Bom));
    let utf16 = reader.read("utf16.txt").expect("utf16");
    assert_eq!(utf16.encoding, Some(TextEncoding::Utf16Le));
    assert_eq!(
        reader.read("binary.bin").expect("binary").kind,
        ContentKind::Binary
    );
    assert_eq!(
        reader.read("large.bin").expect("large").kind,
        ContentKind::TooLarge
    );
}

/// Proves search truncation is visible and polling reports an external edit.
#[test]
fn search_and_polling_detector_report_bounds_and_external_changes() {
    let root = TempDir::create();
    fs::create_dir(root.0.join("nested")).expect("create nested");
    fs::write(
        root.0.join("nested").join("note.txt"),
        "needle\nsecond needle",
    )
    .expect("write note");
    fs::write(root.0.join("binary"), [0, 1, 2]).expect("write binary");
    let handle = workspace(&root);
    let search = TextSearch::new(
        handle.clone(),
        SearchPolicy {
            max_results: 1,
            ..SearchPolicy::default()
        },
    );
    let result = search.search("", "needle").expect("search");
    assert_eq!(result.hits.len(), 1);
    assert!(result.truncated);
    assert_eq!(result.hits[0].line, 1);

    let mut detector = PollingChangeDetector::new(
        handle,
        PollingPolicy {
            min_interval_millis: 1,
            ..PollingPolicy::default()
        },
    )
    .expect("detector");
    std::thread::sleep(Duration::from_millis(3));
    fs::write(root.0.join("nested").join("note.txt"), "needle changed").expect("external edit");
    let batch = detector.poll().expect("poll");
    assert_eq!(batch.state, PollState::Updated);
    assert!(
        batch
            .changes
            .iter()
            .any(|change| change.relative_path == "nested/note.txt")
    );
}

/// Proves an oversized later snapshot stays overflowed until an explicit
/// rescan instead of silently replacing the previous baseline.
#[test]
fn polling_budget_reports_overflow() {
    let root = TempDir::create();
    fs::write(root.0.join("small.txt"), "ok").expect("write small file");
    let handle = workspace(&root);
    let mut detector = PollingChangeDetector::new(
        handle,
        PollingPolicy {
            min_interval_millis: 1,
            max_total_bytes: 16,
            ..PollingPolicy::default()
        },
    )
    .expect("bounded detector");
    fs::write(root.0.join("large.txt"), [1_u8; 128]).expect("write large file");
    std::thread::sleep(Duration::from_millis(3));
    let batch = detector.poll().expect("overflow poll");
    assert_eq!(batch.state, PollState::Overflow);
    assert!(batch.requires_rescan);
    assert!(matches!(
        detector.rescan(),
        Ok(ChangeBatch {
            state: PollState::Overflow,
            requires_rescan: true,
            ..
        })
    ));
}

#[cfg(unix)]
/// Proves links appear as opaque tree entries and cannot be traversed.
#[test]
fn symlink_is_visible_but_never_followed() {
    use std::os::unix::fs::symlink;
    let root = TempDir::create();
    let outside = TempDir::create();
    fs::write(outside.0.join("secret.txt"), "secret").expect("outside file");
    symlink(&outside.0, root.0.join("linked")).expect("directory link");
    let handle = workspace(&root);
    assert!(matches!(
        handle.resolve_directory("linked"),
        Err(WorkspaceError::LinkNotAllowed)
    ));
    let page = TreeReader::new(handle, TreePolicy::default())
        .read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: None,
            page_size: None,
            snapshot_token: None,
        })
        .expect("tree page");
    assert_eq!(page.entries[0].metadata.kind, EntryKind::Symlink);
    assert!(!page.entries[0].can_expand);
}

#[cfg(windows)]
/// Proves Windows reparse links use the same opaque, non-traversable boundary.
#[test]
fn reparse_point_is_visible_but_never_followed() {
    use std::os::windows::fs::symlink_dir;
    let root = TempDir::create();
    let outside = TempDir::create();
    fs::write(outside.0.join("secret.txt"), "secret").expect("outside file");
    if symlink_dir(&outside.0, root.0.join("linked")).is_err() {
        return;
    }
    let handle = workspace(&root);
    assert!(matches!(
        handle.resolve_directory("linked"),
        Err(WorkspaceError::LinkNotAllowed)
    ));
    let page = TreeReader::new(handle, TreePolicy::default())
        .read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: None,
            page_size: None,
            snapshot_token: None,
        })
        .expect("tree page");
    assert_eq!(page.entries[0].metadata.kind, EntryKind::ReparsePoint);
    assert!(!page.entries[0].can_expand);
}

/// Proves root admission does not canonicalize away a raw link identity.
#[cfg(unix)]
#[test]
fn raw_symlink_root_is_rejected() {
    use std::os::unix::fs::symlink;
    let target = TempDir::create();
    let link = target.0.with_extension("root-link");
    symlink(&target.0, &link).expect("create root link");
    assert!(matches!(
        WorkspaceRegistry::default().register(&link),
        Err(WorkspaceError::InvalidRoot)
    ));
    let _ = fs::remove_file(link);
}

/// Proves Windows junction/symlink roots are rejected before canonicalization.
#[cfg(windows)]
#[test]
fn raw_reparse_root_is_rejected() {
    use std::os::windows::fs::symlink_dir;
    let target = TempDir::create();
    let link = target.0.with_extension("root-link");
    if symlink_dir(&target.0, &link).is_err() {
        return;
    }
    assert!(matches!(
        WorkspaceRegistry::default().register(&link),
        Err(WorkspaceError::InvalidRoot)
    ));
    let _ = fs::remove_dir(link);
}

/// Proves a hard-linked file is not treated as an isolated workspace file.
#[test]
fn hard_link_file_is_rejected() {
    let root = TempDir::create();
    let original = root.0.join("original.txt");
    let alias = root.0.join("alias.txt");
    fs::write(&original, "secret").expect("write hardlink source");
    if fs::hard_link(&original, &alias).is_err() {
        return;
    }
    let handle = workspace(&root);
    assert!(matches!(
        handle.resolve_file("alias.txt"),
        Err(WorkspaceError::LinkNotAllowed)
    ));
}

/// Proves hashing remains capped without a deadline, including sparse files,
/// so metadata cannot spend unlimited time reading a file that grew after its
/// initial stat.
#[test]
fn hash_cap_is_bounded_without_deadline() {
    let root = TempDir::create();
    let exact = root.0.join("exact.bin");
    fs::write(&exact, [1_u8; 8]).expect("write exact file");
    assert!(
        super::registry::hash_file_for_test(&exact, 8, None).is_ok(),
        "a file at the limit remains hashable"
    );

    let oversized = root.0.join("oversized.bin");
    fs::write(&oversized, [2_u8; 9]).expect("write oversized file");
    assert!(matches!(
        super::registry::hash_file_for_test(&oversized, 8, None),
        Err(WorkspaceError::FileTooLarge)
    ));

    let sparse = root.0.join("sparse.bin");
    let sparse_file = fs::File::create(&sparse).expect("create sparse file");
    sparse_file.set_len(9).expect("extend sparse file");
    assert!(matches!(
        super::registry::hash_file_for_test(&sparse, 8, None),
        Err(WorkspaceError::FileTooLarge)
    ));
}

/// Proves Windows device names and trailing-dot/space aliases cannot cross
/// the path boundary as a different native target than the UI requested.
#[cfg(windows)]
#[test]
fn windows_device_and_alias_paths_are_rejected() {
    let root = TempDir::create();
    let handle = workspace(&root);
    for path in ["CON.txt", "trailing.", "trailing "] {
        assert!(matches!(
            handle.resolve_file(path),
            Err(WorkspaceError::InvalidRelativePath)
        ));
    }
}

/// Proves a non-UTF-8 native filename cannot be lossy-mapped into an IPC path.
#[cfg(unix)]
#[test]
fn non_utf8_tree_name_is_rejected() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = TempDir::create();
    let name = OsString::from_vec(vec![0xff, b'.', b't', b'x', b't']);
    fs::write(root.0.join(std::path::Path::new(&name)), "opaque").expect("write native name");
    let reader = TreeReader::new(workspace(&root), TreePolicy::default());
    assert!(matches!(
        reader.read_page(&TreePageRequest {
            relative_path: String::new(),
            cursor: None,
            page_size: None,
            snapshot_token: None,
        }),
        Err(WorkspaceError::InvalidRelativePath)
    ));
}

/// Proves a handle cannot continue using a path after the admitted root is
/// replaced by another directory with the same native spelling.
#[test]
fn root_identity_swap_fails_closed() {
    let root = TempDir::create();
    let handle = workspace(&root);
    let old_root = root.0.with_extension("old-root");
    fs::rename(&root.0, &old_root).expect("move admitted root");
    fs::create_dir_all(&root.0).expect("create replacement root");
    assert!(matches!(
        handle.resolve_directory(""),
        Err(WorkspaceError::PathChanged)
    ));
    let _ = fs::remove_dir_all(old_root);
}

/// Proves a captured component identity detects a directory replacement even
/// when the replacement remains within the canonical workspace root.
#[test]
fn resolved_component_swap_fails_closed() {
    let root = TempDir::create();
    fs::create_dir(root.0.join("src")).expect("create source directory");
    fs::write(root.0.join("src").join("main.rs"), "old").expect("write source");
    let handle = workspace(&root);
    let resolved = handle
        .resolve_guard("src/main.rs", Some(false))
        .expect("capture component guard");
    let old = root.0.join("src-old");
    fs::rename(root.0.join("src"), &old).expect("move source directory");
    fs::create_dir(root.0.join("src")).expect("create replacement directory");
    fs::write(root.0.join("src").join("main.rs"), "new").expect("write replacement");
    assert!(matches!(
        handle.verify_resolved(&resolved, Some(false)),
        Err(WorkspaceError::PathChanged)
    ));
    let _ = fs::remove_dir_all(old);
}
