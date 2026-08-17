// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Bounded native process-table evidence for probe-owned temporary roots.

use super::fd;
use super::process::{
    abort_unreaped_query, current_uid, reap_child_bounded, reap_child_without_group,
};
use super::write_scope_report;
use crate::spawn_grouped;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MARKER_MODE: u32 = 0o600;
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;
const QUERY_DEADLINE: Duration = Duration::from_secs(2);
const QUERY_BYTES: usize = 256 * 1024;
const SCOPE_BYTES: usize = 64 * 1024;
const MANIFEST_BYTES: usize = 1024 * 1024;
const QUARANTINE_PREFIX: &str = ".ja-sandbox-probe-scope-quarantine-";
const QUARANTINE_MANIFEST: &str = ".ja-sandbox-probe-scope-quarantine.manifest";
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;

/// Scan probe scope evidence with the same Rust cleanup binary used for
/// markers; no shell process-name heuristic or signal operation is involved.
pub(super) fn run(root: &Path, report: &Path) -> Result<(), &'static str> {
    let mut io = RealEvidenceIo;
    let scopes = match read_scope_files(root) {
        Ok(scopes) => scopes,
        Err(category) => return write_scan_failure(report, category),
    };
    let scope_count = scopes.len();
    let residuals = if scopes.is_empty() {
        0
    } else {
        match process_table_residuals(&scopes) {
            Ok(residuals) => residuals,
            Err(category) => return write_scan_failure(report, category),
        }
    };
    let mut categories = BTreeSet::new();
    if residuals != 0 {
        categories.insert("process-table-residual");
    }
    let quarantine = if residuals == 0 && !scopes.is_empty() {
        // Quarantine is a transaction: every source is validated first, then
        // all moves are published and fsynced before the report can claim a
        // clean process table.  Failures intentionally retain the manifest.
        match quarantine_scope_files(&mut io, root, &scopes) {
            Ok(transaction) => Some(transaction),
            Err(category) => return write_scan_failure(report, category),
        }
    } else {
        None
    };
    if write_scope_report(report, &categories, scope_count).is_err() {
        return Err("process-table-report");
    }
    if let Some(transaction) = quarantine {
        if let Err(category) = finish_quarantine(&mut io, transaction) {
            return write_scan_failure(report, category);
        }
    }
    if categories.is_empty() {
        Ok(())
    } else {
        Err("process table residual")
    }
}

/// Preserve a fixed report category even when the independent process-table
/// query cannot complete; an absent report must never look like a clean scan.
fn write_scan_failure(report: &Path, category: &'static str) -> Result<(), &'static str> {
    let mut categories = BTreeSet::new();
    categories.insert(category);
    super::write_scope_report(report, &categories, 0).map_err(|_| category)?;
    Err(category)
}

/// Discover only owner-private scope files; malformed entries fail closed and
/// remain visible to the outer workflow instead of being silently skipped.
fn read_scope_files(root: &Path) -> Result<Vec<ScopeFile>, &'static str> {
    let metadata = fs::symlink_metadata(root).map_err(|_| "process-table-root")?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != current_uid()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err("process-table-root");
    }
    let entries = fs::read_dir(root).map_err(|_| "process-table-root")?;
    let mut scopes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "process-table-entry")?;
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or("process-table-entry")?;
        // A half-published scope is evidence that an atomic replacement did
        // not complete; accepting the directory as clean would hide the
        // child identity that the outer gate must investigate.
        if is_pending_scope_name(&name) {
            return Err("process-table-scope");
        }
        if is_failure_scope_name(&name) {
            return Err("process-table-failure");
        }
        if is_quarantine_name(&name) {
            return Err("process-table-quarantine");
        }
        if !name.starts_with("ja-sandbox-probe-scope-") || !name.ends_with(".scope") {
            continue;
        }
        let path = entry.path();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
            .open(&path)
            .map_err(|_| "process-table-scope")?;
        let file_identity = validate_scope_file(&file)?;
        let evidence = read_scope(&file)?;
        scopes.push(ScopeFile {
            path,
            entries: evidence.entries,
            contents: evidence.contents,
            file_identity,
        });
    }
    let mut pids = BTreeSet::new();
    let mut process_groups = BTreeSet::new();
    for scope in &scopes {
        for entry in &scope.entries {
            if !pids.insert(entry.pid) || !process_groups.insert(entry.pgid) {
                return Err("process-table-scope");
            }
        }
    }
    Ok(scopes)
}

/// Recognize an incomplete atomic scope publication so a failed rename cannot
/// be mistaken for an empty process table by either native scan or workflow.
fn is_pending_scope_name(name: &str) -> bool {
    name.starts_with("ja-sandbox-probe-scope-") && name.contains(".scope.pending-")
}

/// A persistent registration/cleanup failure is unsafe evidence even when no
/// active scope remains, so the residual gate must report it explicitly.
fn is_failure_scope_name(name: &str) -> bool {
    name.starts_with("ja-sandbox-probe-scope-") && name.ends_with(".scope.failure")
}

/// Treat a retained quarantine or manifest as active failure evidence instead
/// of allowing a later scan to mistake an interrupted transaction for clean.
fn is_quarantine_name(name: &str) -> bool {
    name == QUARANTINE_MANIFEST || name.starts_with(QUARANTINE_PREFIX)
}

/// Validate the descriptor-bound evidence before trusting its identities,
/// preventing symlink/hardlink redirection and owner changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScopeFileIdentity {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
}

fn validate_scope_file(file: &File) -> Result<ScopeFileIdentity, &'static str> {
    let metadata = file.metadata().map_err(|_| "process-table-scope")?;
    let identity = scope_identity_from_metadata(&metadata);
    if !metadata.file_type().is_file()
        || identity.uid != current_uid()
        || identity.mode != MARKER_MODE
        || identity.nlink != 1
    {
        return Err("process-table-scope");
    }
    Ok(identity)
}

/// Re-open a scope immediately before unlink and reject any path/inode swap.
fn validate_scope_path(path: &Path, expected: ScopeFileIdentity) -> Result<(), &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| "process-table-evidence")?;
    let descriptor_identity = validate_scope_file(&file)?;
    let path_metadata = fs::symlink_metadata(path).map_err(|_| "process-table-evidence")?;
    let path_identity = scope_identity_from_metadata(&path_metadata);
    if !scope_identity_matches(
        expected,
        descriptor_identity,
        path_metadata.file_type().is_symlink(),
        path_identity,
    ) {
        return Err("process-table-evidence");
    }
    Ok(())
}

/// Keep the descriptor/path identity rule pure so every delete caller and its
/// reserved/symlink/hardlink fault cases share exactly one predicate.
fn scope_identity_matches(
    expected: ScopeFileIdentity,
    descriptor: ScopeFileIdentity,
    is_symlink: bool,
    path: ScopeFileIdentity,
) -> bool {
    !is_symlink && descriptor == expected && path == descriptor
}

/// Convert both descriptor and path metadata through one representation so an
/// immediate pre-delete lstat cannot disagree with the descriptor fstat.
fn scope_identity_from_metadata(metadata: &fs::Metadata) -> ScopeFileIdentity {
    ScopeFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    }
}

/// Parse one bounded owner-only descriptor.  The root is validated as an
/// auxiliary audit value, while PID/PGID/start/comm identities authorize a
/// process match; no path substring can authorize a signal or residual count.
fn read_scope(file: &File) -> Result<ScopeEvidence, &'static str> {
    let mut contents = Vec::new();
    let bytes = file
        .take((SCOPE_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|_| "process-table-scope")?;
    if bytes > SCOPE_BYTES {
        return Err("process-table-scope");
    }
    let text = std::str::from_utf8(&contents).map_err(|_| "process-table-scope")?;
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.len() < 3 || lines[0] != "scope-version=1" {
        return Err("process-table-scope");
    }
    let root = lines[1]
        .strip_prefix("root=")
        .filter(|value| {
            !value.is_empty()
                && Path::new(value).is_absolute()
                && !value.chars().any(char::is_control)
        })
        .ok_or("process-table-scope")?;
    let _auxiliary_root = root;
    let mut entries = Vec::new();
    for line in &lines[2..] {
        let entry = parse_scope_entry(line)?;
        if entries
            .iter()
            .any(|existing: &ScopeEntry| existing.pid == entry.pid || existing.pgid == entry.pgid)
        {
            return Err("process-table-scope");
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err("process-table-evidence");
    }
    Ok(ScopeEvidence { entries, contents })
}

#[derive(Debug)]
struct ScopeEvidence {
    entries: Vec<ScopeEntry>,
    contents: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
struct ScopeEntry {
    pid: u32,
    pgid: i32,
    start_identity: String,
    comms: Vec<String>,
}

#[derive(Debug)]
struct ScopeFile {
    path: PathBuf,
    entries: Vec<ScopeEntry>,
    contents: Vec<u8>,
    file_identity: ScopeFileIdentity,
}

#[derive(Debug)]
struct QuarantineEntry {
    source: PathBuf,
    quarantine: PathBuf,
    expected: ScopeFileIdentity,
    contents: Vec<u8>,
    moved: bool,
}

#[derive(Debug)]
struct QuarantineTransaction {
    root: PathBuf,
    manifest: PathBuf,
    manifest_identity: ScopeFileIdentity,
    manifest_contents: Vec<u8>,
    entries: Vec<QuarantineEntry>,
}

/// Narrow filesystem boundary shared by production cleanup and injected fault
/// tests so the tests exercise the same transaction functions as production.
trait EvidenceIo {
    fn write_complete(
        &mut self,
        path: &Path,
        image: &[u8],
    ) -> Result<ScopeFileIdentity, &'static str>;
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), &'static str>;
    fn validate(&mut self, path: &Path, expected: ScopeFileIdentity) -> Result<(), &'static str>;
    fn sync_parent(&mut self, path: &Path) -> Result<(), &'static str>;
    fn sync_dir(&mut self, path: &Path) -> Result<(), &'static str>;
    fn unlink(&mut self, path: &Path) -> Result<(), &'static str>;
}

struct RealEvidenceIo;

impl EvidenceIo for RealEvidenceIo {
    fn write_complete(
        &mut self,
        path: &Path,
        image: &[u8],
    ) -> Result<ScopeFileIdentity, &'static str> {
        real_write_manifest_image(path, image)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), &'static str> {
        fs::rename(from, to).map_err(|_| "process-table-quarantine")
    }

    fn validate(&mut self, path: &Path, expected: ScopeFileIdentity) -> Result<(), &'static str> {
        validate_scope_path(path, expected)
    }

    fn sync_parent(&mut self, path: &Path) -> Result<(), &'static str> {
        let parent = path.parent().ok_or("process-table-evidence")?;
        sync_directory(parent)
    }

    fn sync_dir(&mut self, path: &Path) -> Result<(), &'static str> {
        sync_directory(path)
    }

    fn unlink(&mut self, path: &Path) -> Result<(), &'static str> {
        fs::remove_file(path).map_err(|_| "process-table-evidence")
    }
}

/// Build and validate the complete move plan before changing any source path;
/// this prevents a partial scan from deleting the only evidence of a leak.
fn quarantine_scope_files(
    io: &mut impl EvidenceIo,
    root: &Path,
    scopes: &[ScopeFile],
) -> Result<QuarantineTransaction, &'static str> {
    let manifest = root.join(QUARANTINE_MANIFEST);
    match fs::symlink_metadata(&manifest) {
        Ok(_) => return Err("process-table-quarantine"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("process-table-quarantine"),
    }
    let mut entries = Vec::with_capacity(scopes.len());
    let mut preparation_failure = None;
    for (index, scope) in scopes.iter().enumerate() {
        if validate_scope_path(&scope.path, scope.file_identity).is_err() {
            preparation_failure.get_or_insert("process-table-evidence");
            continue;
        }
        let source_name = match scope
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| valid_file_name(value))
        {
            Some(value) => value,
            None => {
                preparation_failure.get_or_insert("process-table-scope");
                continue;
            }
        };
        let quarantine = root.join(format!("{QUARANTINE_PREFIX}{index}-{source_name}"));
        match fs::symlink_metadata(&quarantine) {
            Ok(_) => {
                preparation_failure.get_or_insert("process-table-quarantine");
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                preparation_failure.get_or_insert("process-table-quarantine");
                continue;
            }
        }
        entries.push(QuarantineEntry {
            source: scope.path.clone(),
            quarantine,
            expected: scope.file_identity,
            contents: scope.contents.clone(),
            moved: false,
        });
    }
    if let Some(category) = preparation_failure {
        return Err(category);
    }
    let (manifest_identity, manifest_contents) =
        write_quarantine_manifest(io, &manifest, &entries)?;
    io.sync_dir(root)?;

    let mut failure = None;
    for entry in &mut entries {
        if validate_scope_path(&entry.source, entry.expected).is_err() {
            failure.get_or_insert("process-table-evidence");
            continue;
        }
        if fs::rename(&entry.source, &entry.quarantine).is_err() {
            failure.get_or_insert("process-table-evidence");
        } else {
            entry.moved = true;
        }
    }
    if io.sync_dir(root).is_err() {
        failure.get_or_insert("process-table-evidence");
    }
    if let Some(category) = failure {
        // The manifest and all successfully moved files are deliberately left
        // in place so the outer cleanup can inspect exact residual evidence.
        return Err(category);
    }
    Ok(QuarantineTransaction {
        root: root.to_owned(),
        manifest,
        manifest_identity,
        manifest_contents,
        entries,
    })
}

/// Remove a fully published quarantine only after the clean report is durable;
/// every entry is attempted and failures retain the manifest for diagnosis.
fn finish_quarantine(
    io: &mut impl EvidenceIo,
    transaction: QuarantineTransaction,
) -> Result<(), &'static str> {
    let mut failure = None;
    for entry in &transaction.entries {
        if !entry.moved {
            continue;
        }
        if validate_scope_path(&entry.quarantine, entry.expected).is_err() {
            failure.get_or_insert("process-table-evidence");
            continue;
        }
        if io.unlink(&entry.quarantine).is_err() {
            failure.get_or_insert("process-table-evidence");
        }
    }
    if io.sync_dir(&transaction.root).is_err() {
        failure.get_or_insert("process-table-evidence");
    }
    if failure.is_some() {
        return Err(failure.unwrap_or("process-table-evidence"));
    }
    // The manifest is the last evidence to disappear.  Re-check its
    // descriptor and pathname together immediately before unlink so an
    // inode/path swap cannot turn successful quarantine into silent loss.
    validate_scope_path(&transaction.manifest, transaction.manifest_identity)?;
    io.unlink(&transaction.manifest)?;
    if io.sync_dir(&transaction.root).is_ok() {
        return Ok(());
    }
    match restore_quarantine_manifest(io, &transaction) {
        Ok(()) => Err("process-table-evidence"),
        Err(_) => {
            let persistence = persist_quarantine_failure(io, &transaction.root, "manifest-restore");
            flush_quarantine_evidence_before_abort(&transaction.root);
            if persistence.is_err() {
                eprintln!("SANDBOX-NATIVE: quarantine failure evidence persistence failed");
            }
            std::process::abort()
        }
    }
}

/// Flush the private quarantine directory and diagnostic streams before the
/// deliberate abort; this preserves a durable failure witness at the exact
/// boundary where manifest recovery could no longer be proven.
fn flush_quarantine_evidence_before_abort(root: &Path) {
    let _ = sync_directory(root);
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

/// Write an owner-only full-evidence manifest before any rename.  The complete
/// bounded scope bytes, digest and inode identity make a partial quarantine
/// recoverable even if the original directory entry has already moved.
fn write_quarantine_manifest(
    io: &mut impl EvidenceIo,
    path: &Path,
    entries: &[QuarantineEntry],
) -> Result<(ScopeFileIdentity, Vec<u8>), &'static str> {
    let image = quarantine_manifest_image(entries)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return Err("process-table-quarantine"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("process-table-quarantine"),
    }
    let pending = manifest_pending_path(path)?;
    let pending_identity = write_manifest_image(io, &pending, &image)?;
    let result = (|| {
        io.rename(&pending, path)
            .map_err(|_| "process-table-quarantine")?;
        io.validate(path, pending_identity)
            .map_err(|_| "process-table-quarantine")?;
        let identity = pending_identity;
        io.sync_parent(path)
            .map_err(|_| "process-table-quarantine")?;
        Ok(identity)
    })();
    match result {
        Ok(identity) => Ok((identity, image)),
        Err(error) => {
            // Before rename the pending inode is the only complete image; do
            // not delete it after an uncertain filesystem operation.
            Err(error)
        }
    }
}

/// Write one complete manifest image to a same-directory O_EXCL temporary
/// inode.  The final name is never opened for partial writes.
fn write_manifest_image(
    io: &mut impl EvidenceIo,
    path: &Path,
    image: &[u8],
) -> Result<ScopeFileIdentity, &'static str> {
    io.write_complete(path, image)
}

/// Perform the real owner-only image write used by `RealEvidenceIo`; tests
/// wrap this exact implementation and inject failures at named boundaries.
fn real_write_manifest_image(path: &Path, image: &[u8]) -> Result<ScopeFileIdentity, &'static str> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| "process-table-quarantine")?;
    let initial = validate_scope_file(&file).map_err(|_| "process-table-quarantine")?;
    file.write_all(image)
        .map_err(|_| "process-table-quarantine")?;
    file.sync_all().map_err(|_| "process-table-quarantine")?;
    let identity = validate_scope_file(&file).map_err(|_| "process-table-quarantine")?;
    if identity != initial {
        return Err("process-table-quarantine");
    }
    validate_scope_path(path, identity).map_err(|_| "process-table-quarantine")?;
    Ok(identity)
}

/// Generate a unique same-directory pending name without exposing a partial
/// manifest at the stable path consumed by the process-table scanner.
fn manifest_pending_path(path: &Path) -> Result<PathBuf, &'static str> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "process-table-quarantine")?
        .as_nanos();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("process-table-quarantine")?;
    Ok(path.with_file_name(format!(
        "{QUARANTINE_PREFIX}manifest-pending-{}-{nonce}-{name}",
        std::process::id()
    )))
}

/// Restore the full manifest image through the same atomic publication path;
/// a directory-sync error can never be reported as successful cleanup.
fn restore_quarantine_manifest(
    io: &mut impl EvidenceIo,
    transaction: &QuarantineTransaction,
) -> Result<(), &'static str> {
    let pending = manifest_pending_path(&transaction.manifest)?;
    let pending_identity = write_manifest_image(io, &pending, &transaction.manifest_contents)?;
    match fs::symlink_metadata(&transaction.manifest) {
        Ok(_) => return Err("process-table-evidence"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("process-table-evidence"),
    }
    io.rename(&pending, &transaction.manifest)
        .map_err(|_| "process-table-evidence")?;
    io.validate(&transaction.manifest, pending_identity)
        .map_err(|_| "process-table-evidence")?;
    io.sync_parent(&transaction.manifest)
}

/// Keep an upper-layer fixed failure record if manifest recovery itself is
/// uncertain; the caller aborts so a false success cannot escape the gate.
fn persist_quarantine_failure(
    io: &mut impl EvidenceIo,
    root: &Path,
    category: &'static str,
) -> Result<(), &'static str> {
    if !valid_file_name(category) {
        return Err("process-table-evidence");
    }
    let path = root.join(format!("{QUARANTINE_PREFIX}failure"));
    let image = format!("quarantine-failure-version=1\ncategory={category}\n");
    let _ = write_manifest_image(io, &path, image.as_bytes())?;
    io.sync_parent(&path)
}

/// Build the complete manifest image before opening its path.  This prevents
/// a write error halfway through creation from publishing a partial recovery
/// document before the first quarantine rename.
fn quarantine_manifest_image(entries: &[QuarantineEntry]) -> Result<Vec<u8>, &'static str> {
    let mut image = Vec::new();
    image.extend_from_slice(b"quarantine-version=2\n");
    for entry in entries {
        let source = entry
            .source
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| valid_file_name(value))
            .ok_or("process-table-quarantine")?;
        let quarantine = entry
            .quarantine
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| valid_file_name(value))
            .ok_or("process-table-quarantine")?;
        if entry.contents.len() > SCOPE_BYTES {
            return Err("process-table-quarantine");
        }
        let identity = entry.expected;
        let content_hex = hex_encode(&entry.contents);
        let line = format!(
            "entry\tsource={source}\tquarantine={quarantine}\tdev={}\tino={}\tnlink={}\tmode={}\tuid={}\tcontent_len={}\tcontent_hash={:016x}\tcontent_hex={content_hex}\n",
            identity.dev,
            identity.ino,
            identity.nlink,
            identity.mode,
            identity.uid,
            entry.contents.len(),
            content_digest(&entry.contents),
        );
        if image.len().saturating_add(line.len()) > MANIFEST_BYTES {
            return Err("process-table-quarantine");
        }
        image.extend_from_slice(line.as_bytes());
    }
    Ok(image)
}

/// Encode evidence bytes without allowing tabs/newlines or arbitrary binary
/// data to alter the private manifest grammar.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Use a deterministic bounded digest in the evidence image without adding a
/// crypto dependency to this diagnostic-only cleanup binary.  The raw hex
/// bytes remain the recovery source; this digest detects accidental mutation.
fn content_digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        hash.wrapping_mul(0x100000001b3_u64) ^ u64::from(*byte)
    })
}

/// Synchronize the private parent directory after each rename/unlink batch so
/// a crash cannot report success while the quarantine publication is lost.
fn sync_directory(root: &Path) -> Result<(), &'static str> {
    fd::sync_directory(&File::open(root).map_err(|_| "process-table-evidence")?)
        .map_err(|_| "process-table-evidence")
}

/// Restrict manifest names to the same ASCII grammar used by source scope
/// names; the transaction never interpolates arbitrary path bytes.
fn valid_file_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
}

/// Parse the fixed tab-separated evidence format and reject duplicate or
/// Unicode identity fields before the process table can be consulted.
fn parse_scope_entry(line: &str) -> Result<ScopeEntry, &'static str> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != 5 || fields[0] != "entry" {
        return Err("process-table-scope");
    }
    let pid = fields[1]
        .strip_prefix("pid=")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 1)
        .ok_or("process-table-scope")?;
    let pgid = fields[2]
        .strip_prefix("pgid=")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value > 1)
        .ok_or("process-table-scope")?;
    let start_identity = fields[3]
        .strip_prefix("start_identity=")
        .filter(|value| valid_start_identity(value))
        .ok_or("process-table-scope")?
        .to_owned();
    let comms = fields[4]
        .strip_prefix("comm=")
        .map(|value| value.split('|').map(str::to_owned).collect::<Vec<_>>())
        .filter(|values| {
            !values.is_empty()
                && values.iter().all(|value| valid_comm(value))
                && values.windows(2).all(|pair| pair[0] != pair[1])
        })
        .ok_or("process-table-scope")?;
    Ok(ScopeEntry {
        pid,
        pgid,
        start_identity,
        comms,
    })
}

/// Keep the identity evidence ASCII and delimiter-free so Unicode look-alike
/// fields cannot change the meaning of a trusted PID record.
fn valid_start_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character == ' ' || character.is_ascii_graphic())
        && !value.contains('=')
        && !value.contains('\t')
        && !value.contains('\r')
        && !value.contains('\n')
}

/// Keep process command names in a narrow ASCII grammar; arguments remain
/// opaque because the real fixture intentionally uses Unicode workspace paths.
fn valid_comm(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._/-".contains(character))
}

/// Run a bounded `/bin/ps` query and compare structured rows to private
/// identities; direct child/group cleanup remains mandatory on every error.
fn process_table_residuals(scopes: &[ScopeFile]) -> Result<usize, &'static str> {
    let mut command = Command::new("/bin/ps");
    command
        .args(["-axo", "pid=,pgid=,comm=,lstart=,args="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_grouped(&mut command).map_err(|_| "process-table-query")?;
    let process_group = match i32::try_from(child.id()).ok().filter(|value| *value > 1) {
        Some(value) => value,
        None => {
            if !reap_child_without_group(&mut child) {
                abort_unreaped_query("process-table-invalid-group");
            }
            return Err("process-table-query");
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            finish_query_child(&mut child, process_group)?;
            return Err("process-table-query");
        }
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            finish_query_child(&mut child, process_group)?;
            return Err("process-table-query");
        }
    };
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        drop(stdout);
        drop(stderr);
        finish_query_child(&mut child, process_group)?;
        return Err("process-table-query");
    }
    let deadline = Instant::now() + QUERY_DEADLINE;
    let mut output = Vec::new();
    let mut stdout_bytes = 0_usize;
    let mut stderr_bytes = 0_usize;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    let mut query_error = None;
    loop {
        match drain_pipe(
            &mut stdout,
            &mut output,
            &mut stdout_bytes,
            QUERY_BYTES,
            deadline,
        ) {
            Ok(value) => stdout_eof |= value,
            Err(error) => {
                query_error = Some(error);
                break;
            }
        }
        match drain_pipe_discard(&mut stderr, &mut stderr_bytes, QUERY_BYTES, deadline) {
            Ok(value) => stderr_eof |= value,
            Err(error) => {
                query_error = Some(error);
                break;
            }
        }
        match child.try_wait() {
            Ok(Some(value)) => {
                status = Some(value);
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => break,
            Err(_) => {
                query_error = Some("process-table-query");
                break;
            }
        }
    }
    if query_error.is_none() && status.is_some() {
        while !(stdout_eof && stderr_eof) && Instant::now() < deadline {
            let before = (stdout_eof, stderr_eof, stdout_bytes, stderr_bytes);
            if !stdout_eof {
                match drain_pipe(
                    &mut stdout,
                    &mut output,
                    &mut stdout_bytes,
                    QUERY_BYTES,
                    deadline,
                ) {
                    Ok(value) => stdout_eof |= value,
                    Err(error) => {
                        query_error = Some(error);
                        break;
                    }
                }
            }
            if !stderr_eof {
                match drain_pipe_discard(&mut stderr, &mut stderr_bytes, QUERY_BYTES, deadline) {
                    Ok(value) => stderr_eof |= value,
                    Err(error) => {
                        query_error = Some(error);
                        break;
                    }
                }
            }
            if before == (stdout_eof, stderr_eof, stdout_bytes, stderr_bytes) {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    drop(stdout);
    drop(stderr);
    finish_query_child(&mut child, process_group)?;
    if let Some(error) = query_error {
        return Err(error);
    }
    if !(status.is_some_and(|value| value.success()) && stdout_eof && stderr_eof) {
        return Err("process-table-query");
    }
    count_scope_matches(&output, scopes)
}

/// Close a process-table query only after direct reap and group ESRCH are
/// proven; the shared finalizer aborts if ownership cannot be established.
fn finish_query_child(child: &mut Child, process_group: i32) -> Result<(), &'static str> {
    reap_child_bounded(child, process_group, Instant::now() + QUERY_DEADLINE)
        .map_err(|_| "process-table-query")
}

#[derive(Debug, Eq, PartialEq)]
struct ProcessRow {
    pid: u32,
    pgid: i32,
    comm: String,
    start_identity: String,
    args: String,
}

/// Parse every nonempty `ps` row into fixed columns; malformed or ambiguous
/// output fails closed instead of becoming a false clean residual scan.
fn parse_process_table(output: &[u8]) -> Result<Vec<ProcessRow>, &'static str> {
    let text = std::str::from_utf8(output).map_err(|_| "process-table-parse")?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_process_row)
        .collect()
}

/// Parse PID, PGID, comm and the five-token `lstart` identity; args are kept
/// opaque so Unicode fixture paths remain valid but never authorize a match.
fn parse_process_row(line: &str) -> Result<ProcessRow, &'static str> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 8 {
        return Err("process-table-parse");
    }
    let pid = fields[0]
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 1)
        .ok_or("process-table-parse")?;
    let pgid = fields[1]
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 1)
        .ok_or("process-table-parse")?;
    let comm = fields[2].to_owned();
    if !valid_comm(&comm) {
        return Err("process-table-parse");
    }
    let start_identity = fields[3..8].join(" ");
    if !valid_start_identity(&start_identity) {
        return Err("process-table-parse");
    }
    Ok(ProcessRow {
        pid,
        pgid,
        comm,
        start_identity,
        args: fields[8..].join(" "),
    })
}

/// Match exact PID identities first, then trusted PGID descendants.  A path
/// token in `args` is only auxiliary evidence and never makes an unrelated
/// same-scope process count as a controlled residual.
fn count_scope_matches(output: &[u8], scopes: &[ScopeFile]) -> Result<usize, &'static str> {
    let rows = parse_process_table(output)?;
    let mut count = 0;
    for row in rows {
        let pid_entries = scopes
            .iter()
            .flat_map(|scope| scope.entries.iter())
            .filter(|entry| entry.pid == row.pid)
            .collect::<Vec<_>>();
        if !pid_entries.is_empty() {
            if pid_entries.iter().any(|entry| {
                entry.pgid == row.pgid
                    && entry.start_identity == row.start_identity
                    && entry.comms.iter().any(|comm| comm == &row.comm)
            }) {
                count += 1;
                continue;
            }
            return Err("process-table-identity");
        }
        if scopes
            .iter()
            .any(|scope| scope.entries.iter().any(|entry| entry.pgid == row.pgid))
        {
            count += 1;
        }
    }
    Ok(count)
}

/// Drain one nonblocking stdout with an independent cumulative byte cap and
/// shared absolute deadline; the counter intentionally outlives each drain.
fn drain_pipe<R: Read>(
    reader: &mut R,
    output: &mut Vec<u8>,
    consumed: &mut usize,
    cap: usize,
    deadline: Instant,
) -> Result<bool, &'static str> {
    let mut buffer = [0_u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                *consumed = consumed.saturating_add(read);
                if *consumed > cap {
                    return Err("process-table-output");
                }
                output.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(_) => return Err("process-table-query"),
        }
    }
}

/// Drain stderr under its own cumulative cap so a noisy helper cannot reset
/// its budget on every poll or hide a live Child behind unbounded output.
fn drain_pipe_discard<R: Read>(
    reader: &mut R,
    consumed: &mut usize,
    cap: usize,
    deadline: Instant,
) -> Result<bool, &'static str> {
    let mut buffer = [0_u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                *consumed = consumed.saturating_add(read);
                if *consumed > cap {
                    return Err("process-table-output");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(_) => return Err("process-table-query"),
        }
    }
}

/// Set a pipe nonblocking before any bounded read, so descendant-held writers
/// cannot turn the independent process-table evidence into an unbounded join.
fn set_nonblocking<R: AsRawFd>(reader: &R) -> io::Result<()> {
    let flags = unsafe { fcntl(reader.as_raw_fd(), F_GETFL) };
    if flags == -1 || unsafe { fcntl(reader.as_raw_fd(), F_SETFL, flags | O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use super::{
        MARKER_MODE, QUARANTINE_MANIFEST, QUARANTINE_PREFIX, QuarantineEntry,
        QuarantineTransaction, RealEvidenceIo, ScopeEntry, ScopeFile, ScopeFileIdentity,
        content_digest, count_scope_matches, drain_pipe, drain_pipe_discard, finish_quarantine,
        hex_encode, is_failure_scope_name, is_pending_scope_name, is_quarantine_name,
        manifest_pending_path, parse_scope_entry, quarantine_manifest_image,
        restore_quarantine_manifest, scope_identity_matches, validate_scope_path,
        write_manifest_image, write_quarantine_manifest,
    };
    use crate::run_bounded_command;
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{self, Read};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn scope() -> Vec<ScopeFile> {
        vec![ScopeFile {
            path: PathBuf::from("/tmp/unused.scope"),
            entries: vec![ScopeEntry {
                pid: 42,
                pgid: 42,
                start_identity: "Mon Jan 1 00:00:00 2026".to_owned(),
                comms: vec!["ja-sandbox-worker".to_owned()],
            }],
            contents: b"scope-version=1\nroot=/tmp\nentry\tpid=42\tpgid=42\tstart_identity=Mon Jan 1 00:00:00 2026\tcomm=ja-sandbox-worker\n".to_vec(),
            file_identity: ScopeFileIdentity {
                dev: 1,
                ino: 1,
                nlink: 1,
                mode: 0o600,
                uid: 1,
            },
        }]
    }

    fn row(pid: u32, pgid: i32, comm: &str, start: &str, args: &str) -> String {
        format!("{pid} {pgid} {comm} {start} {args}")
    }

    /// A row with the exact evidence identity is controlled even when args
    /// contain Unicode fixture paths.
    #[test]
    fn exact_identity_counts() {
        let output = row(
            42,
            42,
            "ja-sandbox-worker",
            "Mon Jan 1 00:00:00 2026",
            "/tmp/ja-scope-unique/工作区/worker",
        );
        assert_eq!(count_scope_matches(output.as_bytes(), &scope()), Ok(1));
    }

    /// Same-path command arguments do not authorize an unrelated PID or
    /// process group, preventing the historical line-substring false match.
    #[test]
    fn same_scope_args_unrelated_process_is_ignored() {
        let output = row(
            99,
            99,
            "other-process",
            "Mon Jan 1 00:00:00 2026",
            "/tmp/ja-scope-unique/worker",
        );
        assert_eq!(count_scope_matches(output.as_bytes(), &scope()), Ok(0));
    }

    /// A reused PID with a different start identity fails closed rather than
    /// being treated as the old worker.
    #[test]
    fn pid_reuse_start_mismatch_fails_closed() {
        let output = row(
            42,
            42,
            "ja-sandbox-worker",
            "Tue Jan 2 00:00:00 2026",
            "worker",
        );
        assert_eq!(
            count_scope_matches(output.as_bytes(), &scope()),
            Err("process-table-identity")
        );
    }

    /// A reused PID with a different PGID or comm must never be signalled or
    /// counted as the controlled process.
    #[test]
    fn pgid_and_comm_mismatch_fail_closed() {
        let pgid_output = row(
            42,
            43,
            "ja-sandbox-worker",
            "Mon Jan 1 00:00:00 2026",
            "worker",
        );
        assert_eq!(
            count_scope_matches(pgid_output.as_bytes(), &scope()),
            Err("process-table-identity")
        );
        let comm_output = row(42, 42, "unrelated", "Mon Jan 1 00:00:00 2026", "worker");
        assert_eq!(
            count_scope_matches(comm_output.as_bytes(), &scope()),
            Err("process-table-identity")
        );
    }

    /// Malformed and Unicode identity columns are rejected while the parser
    /// still permits Unicode only in opaque command arguments.
    #[test]
    fn malformed_or_unicode_identity_fails_closed() {
        let malformed = b"42 42 ja-sandbox-worker Mon Jan 1\n";
        assert_eq!(
            count_scope_matches(malformed, &scope()),
            Err("process-table-parse")
        );
        let unicode_comm = row(42, 42, "工作进程", "Mon Jan 1 00:00:00 2026", "worker");
        assert_eq!(
            count_scope_matches(unicode_comm.as_bytes(), &scope()),
            Err("process-table-parse")
        );
    }

    /// A pending scope sibling is an unsafe publication state, even though it
    /// does not yet contain a complete identity record.
    #[test]
    fn pending_scope_name_is_not_clean_evidence() {
        assert!(is_pending_scope_name(
            "ja-sandbox-probe-scope-42-7.scope.pending-42-8"
        ));
        assert!(!is_pending_scope_name("ja-sandbox-probe-scope-42-7.scope"));
        assert!(!is_pending_scope_name("unrelated.scope.pending-42-8"));
        assert!(is_failure_scope_name(
            "ja-sandbox-probe-scope-42-7.scope.failure"
        ));
        assert!(is_quarantine_name(QUARANTINE_MANIFEST));
        assert!(is_quarantine_name(
            ".ja-sandbox-probe-scope-quarantine-0-ja-sandbox-probe-scope-42-7.scope"
        ));
        assert!(!is_quarantine_name("ja-sandbox-probe-scope-42-7.scope"));
    }

    /// Scope parsing rejects reserved and signed IDs before any process table
    /// result can be treated as a clean residual scan.
    #[test]
    fn reserved_scope_ids_fail_closed() {
        for pid in ["-1", "0", "1"] {
            let line = format!(
                "entry\tpid={pid}\tpgid=42\tstart_identity=Mon Jan 1 00:00:00 2026\tcomm=ja-sandbox-worker"
            );
            assert_eq!(parse_scope_entry(&line), Err("process-table-scope"));
        }
        for pgid in ["-1", "0", "1"] {
            let line = format!(
                "entry\tpid=42\tpgid={pgid}\tstart_identity=Mon Jan 1 00:00:00 2026\tcomm=ja-sandbox-worker"
            );
            assert_eq!(parse_scope_entry(&line), Err("process-table-scope"));
        }
    }

    /// Every descriptor/path field participates in the immediate delete
    /// predicate; a symlink, hardlink count, or inode mismatch is never gone.
    #[test]
    fn scope_path_identity_faults_fail_closed() {
        let expected = ScopeFileIdentity {
            dev: 1,
            ino: 2,
            nlink: 1,
            mode: 0o600,
            uid: 3,
        };
        assert!(scope_identity_matches(expected, expected, false, expected));
        for (descriptor, path, symlink) in [
            (ScopeFileIdentity { ino: 9, ..expected }, expected, false),
            (
                expected,
                ScopeFileIdentity {
                    nlink: 2,
                    ..expected
                },
                false,
            ),
            (expected, expected, true),
        ] {
            assert!(!scope_identity_matches(expected, descriptor, symlink, path));
        }
    }

    /// The pre-rename manifest contains bounded full evidence, not just a
    /// filename, so a partial quarantine remains recoverable and auditable.
    #[test]
    fn quarantine_manifest_contains_full_evidence() {
        let contents = b"scope-version=1\nroot=/private\n".to_vec();
        let entry = QuarantineEntry {
            source: PathBuf::from("/private/scope.scope"),
            quarantine: PathBuf::from("/private/.ja-sandbox-probe-scope-quarantine-0-scope.scope"),
            expected: ScopeFileIdentity {
                dev: 7,
                ino: 8,
                nlink: 1,
                mode: 0o600,
                uid: 9,
            },
            contents: contents.clone(),
            moved: false,
        };
        let image = quarantine_manifest_image(&[entry]).expect("manifest image");
        let text = String::from_utf8(image).expect("manifest utf8");
        assert!(text.starts_with("quarantine-version=2\n"));
        assert!(text.contains(&format!("content_len={}", contents.len())));
        assert!(text.contains(&format!("content_hash={:016x}", content_digest(&contents))));
        assert!(text.contains(&format!("content_hex={}", hex_encode(&contents))));
    }

    /// The stable manifest is published only after a complete same-directory
    /// temporary image is synced; no pending manifest remains after success.
    /// The source scope is intentionally still present here because the outer
    /// quarantine transaction moves it only after this publication phase.
    #[test]
    fn quarantine_manifest_publication_is_atomic() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-atomic-{nonce}"));
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let source = root.join("scope.scope");
        let source_cleanup = source.clone();
        let quarantine = root.join(".ja-sandbox-probe-scope-quarantine-0-scope.scope");
        let contents = b"scope-version=1\nroot=/private\n".to_vec();
        fs::write(&source, &contents).expect("source");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("source mode");
        let metadata = fs::metadata(&source).expect("source metadata");
        let entry = QuarantineEntry {
            source,
            quarantine,
            expected: ScopeFileIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                mode: metadata.permissions().mode() & 0o777,
                uid: metadata.uid(),
            },
            contents,
            moved: false,
        };
        let manifest = root.join(QUARANTINE_MANIFEST);
        let mut io = RealEvidenceIo;
        let (identity, image) =
            write_quarantine_manifest(&mut io, &manifest, &[entry]).expect("publish");
        assert!(validate_scope_path(&manifest, identity).is_ok());
        assert_eq!(fs::read(&manifest).expect("manifest bytes"), image);
        let manifest_names = fs::read_dir(&root)
            .expect("entries")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name == QUARANTINE_MANIFEST
                    || name.starts_with(&format!("{QUARANTINE_PREFIX}manifest-pending-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(manifest_names.len(), 1);
        assert_eq!(manifest_names[0], QUARANTINE_MANIFEST);
        assert!(
            source_cleanup.exists(),
            "source moved before manifest publication"
        );
        fs::remove_file(manifest).expect("manifest cleanup");
        fs::remove_file(source_cleanup).expect("source cleanup");
        fs::remove_dir(root).expect("root cleanup");
    }

    /// A pre-existing final name cannot be replaced by a partial or unrelated
    /// image; publication fails before the temporary inode is created.
    #[test]
    fn quarantine_manifest_existing_final_is_fail_closed() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-collision-{nonce}"));
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let manifest = root.join(QUARANTINE_MANIFEST);
        fs::write(&manifest, b"sentinel\n").expect("sentinel");
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).expect("mode");
        let mut io = RealEvidenceIo;
        let result = write_quarantine_manifest(&mut io, &manifest, &[]);
        assert_eq!(result, Err("process-table-quarantine"));
        assert_eq!(fs::read(&manifest).expect("sentinel bytes"), b"sentinel\n");
        let pending = manifest_pending_path(&manifest).expect("pending name");
        assert!(is_quarantine_name(
            pending
                .file_name()
                .and_then(|value| value.to_str())
                .expect("name")
        ));
        fs::remove_file(manifest).expect("cleanup");
        fs::remove_dir(root).expect("root cleanup");
    }

    /// The held manifest image is sufficient to restore the final inode after
    /// an unlink/directory-sync fault; cleanup must not invent a new image.
    #[test]
    fn quarantine_manifest_restore_uses_held_identity_image() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-restore-{nonce}"));
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let manifest = root.join(QUARANTINE_MANIFEST);
        let image = b"quarantine-version=2\nheld=true\n".to_vec();
        let transaction = QuarantineTransaction {
            root: root.clone(),
            manifest: manifest.clone(),
            manifest_identity: ScopeFileIdentity {
                dev: 0,
                ino: 0,
                nlink: 1,
                mode: 0o600,
                uid: 0,
            },
            manifest_contents: image.clone(),
            entries: Vec::new(),
        };
        let mut io = RealEvidenceIo;
        restore_quarantine_manifest(&mut io, &transaction).expect("restore manifest");
        assert_eq!(fs::read(&manifest).expect("restored manifest"), image);
        fs::remove_file(manifest).expect("cleanup manifest");
        fs::remove_dir(root).expect("cleanup root");
    }

    /// Drive the production image writer, publication, unlink and restore
    /// transactions through real temporary files while injecting each IO
    /// boundary; every failure leaves evidence or returns an error.
    #[test]
    fn production_evidence_io_faults_retain_evidence() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-io-faults-{nonce}"));
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let image = b"quarantine-version=2\nfull=true\n";

        for (index, point) in [ManifestFaultPoint::ShortWrite, ManifestFaultPoint::FileSync]
            .into_iter()
            .enumerate()
        {
            let pending = root.join(format!("fault-image-{index}.pending"));
            let mut io = FaultPlan::new(point);
            assert!(write_manifest_image(&mut io, &pending, image).is_err());
            assert_eq!(fs::read(&pending).expect("retained pending"), image);
            fs::remove_file(pending).expect("pending cleanup");
        }

        let entries: Vec<QuarantineEntry> = Vec::new();
        let rename_manifest = root.join("rename.manifest");
        let mut rename_io = FaultPlan::new(ManifestFaultPoint::Rename);
        assert!(write_quarantine_manifest(&mut rename_io, &rename_manifest, &entries).is_err());
        assert!(!rename_manifest.exists());
        let pending_names = fs::read_dir(&root)
            .expect("pending entries")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.starts_with(QUARANTINE_PREFIX))
            })
            .collect::<Vec<_>>();
        assert_eq!(pending_names.len(), 1);
        fs::remove_file(&pending_names[0]).expect("rename pending cleanup");

        let sync_manifest = root.join("sync.manifest");
        let mut sync_io = FaultPlan::new(ManifestFaultPoint::DirectorySync);
        assert!(write_quarantine_manifest(&mut sync_io, &sync_manifest, &entries).is_err());
        assert_eq!(
            fs::read(&sync_manifest).expect("published image"),
            image_header()
        );
        fs::remove_file(sync_manifest).expect("sync manifest cleanup");

        let unlink_manifest = root.join("unlink.manifest");
        let mut real_io = RealEvidenceIo;
        let (unlink_identity, unlink_image) =
            write_quarantine_manifest(&mut real_io, &unlink_manifest, &entries)
                .expect("unlink fixture");
        let unlink_transaction = QuarantineTransaction {
            root: root.clone(),
            manifest: unlink_manifest.clone(),
            manifest_identity: unlink_identity,
            manifest_contents: unlink_image,
            entries: Vec::new(),
        };
        let mut unlink_io = FaultPlan::new(ManifestFaultPoint::Unlink);
        assert!(finish_quarantine(&mut unlink_io, unlink_transaction).is_err());
        assert!(unlink_manifest.exists());
        fs::remove_file(unlink_manifest).expect("unlink manifest cleanup");

        let durability_manifest = root.join("durability.manifest");
        let mut durability_writer = RealEvidenceIo;
        let (durability_identity, durability_image) =
            write_quarantine_manifest(&mut durability_writer, &durability_manifest, &[])
                .expect("durability fixture");
        let durability_transaction = QuarantineTransaction {
            root: root.clone(),
            manifest: durability_manifest.clone(),
            manifest_identity: durability_identity,
            manifest_contents: durability_image.clone(),
            entries: Vec::new(),
        };
        let mut durability_io = FaultPlan::new(ManifestFaultPoint::ManifestDirectorySync);
        assert!(finish_quarantine(&mut durability_io, durability_transaction).is_err());
        assert_eq!(
            fs::read(&durability_manifest).expect("restored durability manifest"),
            durability_image
        );
        fs::remove_file(durability_manifest).expect("durability manifest cleanup");

        for point in [
            ManifestFaultPoint::RestoreWrite,
            ManifestFaultPoint::RestoreFileSync,
        ] {
            let restore_manifest = root.join("restore.manifest");
            let restore_transaction = QuarantineTransaction {
                root: root.clone(),
                manifest: restore_manifest.clone(),
                manifest_identity: ScopeFileIdentity {
                    dev: 0,
                    ino: 0,
                    nlink: 1,
                    mode: MARKER_MODE,
                    uid: 0,
                },
                manifest_contents: image.to_vec(),
                entries: Vec::new(),
            };
            let mut restore_io = FaultPlan::new(point);
            assert!(restore_quarantine_manifest(&mut restore_io, &restore_transaction).is_err());
            assert!(!restore_manifest.exists());
            let retained = fs::read_dir(&root)
                .expect("restore entries")
                .map(|entry| entry.expect("entry").path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.starts_with(QUARANTINE_PREFIX))
                })
                .collect::<Vec<_>>();
            assert_eq!(retained.len(), 1);
            fs::remove_file(&retained[0]).expect("restore pending cleanup");
        }
        fs::remove_dir(root).expect("root cleanup");
    }

    const MANIFEST_ABORT_ROOT: &str = "JA_SANDBOX_MANIFEST_ABORT_ROOT";

    /// Enter the real manifest finalizer in a separate test process so the
    /// production fixed abort can be observed without terminating the parent
    /// harness; the child must leave a durable failure witness behind.
    #[test]
    fn manifest_post_unlink_restore_failure_is_fixed_abort() {
        if let Some(root) = std::env::var_os(MANIFEST_ABORT_ROOT) {
            run_manifest_abort_child(&PathBuf::from(root));
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-abort-{nonce}"));
        fs::create_dir(&root).expect("abort root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("abort root mode");
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command
            .arg("manifest_post_unlink_restore_failure_is_fixed_abort")
            .arg("--nocapture")
            .env(MANIFEST_ABORT_ROOT, &root);
        let output = run_bounded_command(command, Duration::from_secs(5), 16 * 1024, 16 * 1024)
            .expect("bounded abort fixture");
        assert!(
            !output.status.success(),
            "fault child unexpectedly succeeded"
        );
        let failure = root.join(format!("{QUARANTINE_PREFIX}failure"));
        assert!(failure.is_file(), "fixed failure witness missing");
        let failure_text = fs::read_to_string(&failure).expect("failure witness");
        assert!(failure_text.contains("category=manifest-restore\n"));
        let pending = fs::read_dir(&root)
            .expect("abort evidence entries")
            .map(|entry| entry.expect("abort evidence entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.contains("manifest-pending-"))
            })
            .count();
        assert_eq!(pending, 1, "restore evidence was not retained");
        fs::remove_dir_all(root).expect("abort root cleanup");
    }

    /// Build the exact production transaction that reaches post-unlink
    /// directory-sync failure, restore-write failure, durable marker creation,
    /// and the fixed abort. Returning would make this fixture fail closed.
    fn run_manifest_abort_child(root: &Path) -> ! {
        let manifest = root.join(QUARANTINE_MANIFEST);
        let mut writer = RealEvidenceIo;
        let (identity, image) = write_quarantine_manifest(&mut writer, &manifest, &[])
            .expect("abort manifest publication");
        let transaction = QuarantineTransaction {
            root: root.to_owned(),
            manifest,
            manifest_identity: identity,
            manifest_contents: image,
            entries: Vec::new(),
        };
        let mut io = FaultPlan::with_points([
            ManifestFaultPoint::ManifestDirectorySync,
            ManifestFaultPoint::RestoreWrite,
        ]);
        let _ = finish_quarantine(&mut io, transaction);
        panic!("manifest finalizer returned after unrecoverable restore");
    }

    /// Keep the production header image stable for the publication fault
    /// assertion without duplicating the writer's full manifest serializer.
    fn image_header() -> Vec<u8> {
        b"quarantine-version=2\n".to_vec()
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ManifestFaultPoint {
        ShortWrite,
        FileSync,
        Rename,
        DirectorySync,
        ManifestDirectorySync,
        Unlink,
        RestoreWrite,
        RestoreFileSync,
    }

    /// Inject one bounded filesystem failure after the real operation has
    /// created its evidence image; this keeps residual assertions meaningful.
    struct FaultPlan {
        points: Vec<ManifestFaultPoint>,
        real: RealEvidenceIo,
        sync_dir_calls: usize,
    }

    impl FaultPlan {
        fn new(point: ManifestFaultPoint) -> Self {
            Self::with_points([point])
        }

        fn with_points(points: impl IntoIterator<Item = ManifestFaultPoint>) -> Self {
            Self {
                points: points.into_iter().collect(),
                real: RealEvidenceIo,
                sync_dir_calls: 0,
            }
        }

        fn fails(&self, point: ManifestFaultPoint) -> bool {
            self.points.contains(&point)
        }
    }

    impl super::EvidenceIo for FaultPlan {
        fn write_complete(
            &mut self,
            path: &Path,
            image: &[u8],
        ) -> Result<ScopeFileIdentity, &'static str> {
            let identity = self.real.write_complete(path, image)?;
            if self.fails(ManifestFaultPoint::ShortWrite) {
                return Err("process-table-quarantine");
            }
            if self.fails(ManifestFaultPoint::FileSync) {
                return Err("process-table-quarantine");
            }
            let is_restore_image = path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.contains("manifest-pending-"));
            if is_restore_image && self.fails(ManifestFaultPoint::RestoreWrite) {
                return Err("process-table-evidence");
            }
            if is_restore_image && self.fails(ManifestFaultPoint::RestoreFileSync) {
                return Err("process-table-evidence");
            }
            Ok(identity)
        }

        fn rename(&mut self, from: &Path, to: &Path) -> Result<(), &'static str> {
            if self.fails(ManifestFaultPoint::Rename) {
                return Err("process-table-quarantine");
            }
            self.real.rename(from, to)
        }

        fn validate(
            &mut self,
            path: &Path,
            expected: ScopeFileIdentity,
        ) -> Result<(), &'static str> {
            self.real.validate(path, expected)
        }

        fn sync_parent(&mut self, path: &Path) -> Result<(), &'static str> {
            if self.fails(ManifestFaultPoint::DirectorySync) {
                return Err("process-table-evidence");
            }
            if self.fails(ManifestFaultPoint::RestoreFileSync) {
                return Err("process-table-evidence");
            }
            self.real.sync_parent(path)
        }

        fn sync_dir(&mut self, path: &Path) -> Result<(), &'static str> {
            self.sync_dir_calls = self.sync_dir_calls.saturating_add(1);
            if self.fails(ManifestFaultPoint::DirectorySync) {
                return Err("process-table-evidence");
            }
            if self.fails(ManifestFaultPoint::ManifestDirectorySync) && self.sync_dir_calls >= 2 {
                return Err("process-table-evidence");
            }
            self.real.sync_dir(path)
        }

        fn unlink(&mut self, path: &Path) -> Result<(), &'static str> {
            if self.fails(ManifestFaultPoint::Unlink) {
                return Err("process-table-evidence");
            }
            self.real.unlink(path)
        }
    }

    /// A partial deletion keeps the manifest after the first entry is removed;
    /// cleanup must not claim all evidence disappeared when a later unlink is
    /// missing or otherwise unprovable.
    #[test]
    fn partial_quarantine_retains_manifest() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-quarantine-fault-{nonce}"));
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let quarantine = root.join(".ja-sandbox-probe-scope-quarantine-0-scope.scope");
        fs::write(&quarantine, b"scope").expect("quarantine");
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o600)).expect("file mode");
        let manifest = root.join(QUARANTINE_MANIFEST);
        fs::write(&manifest, b"quarantine-version=2\n").expect("manifest");
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).expect("manifest mode");
        let identity = |path: &Path| {
            let metadata = fs::metadata(path).expect("metadata");
            ScopeFileIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                mode: metadata.permissions().mode() & 0o777,
                uid: metadata.uid(),
            }
        };
        let transaction = QuarantineTransaction {
            root: root.clone(),
            manifest: manifest.clone(),
            manifest_identity: identity(&manifest),
            manifest_contents: b"quarantine-version=2\n".to_vec(),
            entries: vec![
                QuarantineEntry {
                    source: root.join("scope.scope"),
                    quarantine: quarantine.clone(),
                    expected: identity(&quarantine),
                    contents: b"scope".to_vec(),
                    moved: true,
                },
                QuarantineEntry {
                    source: root.join("missing.scope"),
                    quarantine: root.join(".ja-sandbox-probe-scope-quarantine-1-missing.scope"),
                    expected: identity(&quarantine),
                    contents: b"missing".to_vec(),
                    moved: true,
                },
            ],
        };
        let mut io = RealEvidenceIo;
        assert_eq!(
            finish_quarantine(&mut io, transaction),
            Err("process-table-evidence")
        );
        assert!(manifest.exists());
        let _ = fs::remove_file(manifest);
        let _ = fs::remove_dir(root);
    }

    struct ScriptedReader {
        chunks: VecDeque<Result<Vec<u8>, io::ErrorKind>>,
    }

    impl Read for ScriptedReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            match self.chunks.pop_front() {
                Some(Ok(chunk)) => {
                    target[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                Some(Err(kind)) => Err(io::Error::new(kind, "scripted")),
                None => Ok(0),
            }
        }
    }

    /// The stdout budget is cumulative across multiple nonblocking polls;
    /// continuous output cannot reset the cap by returning WouldBlock first.
    #[test]
    fn stdout_cap_accumulates_across_drains() {
        let mut reader = ScriptedReader {
            chunks: VecDeque::from([
                Ok(b"12345678".to_vec()),
                Err(io::ErrorKind::WouldBlock),
                Ok(b"9".to_vec()),
            ]),
        };
        let mut output = Vec::new();
        let mut consumed = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            drain_pipe(&mut reader, &mut output, &mut consumed, 8, deadline),
            Ok(false)
        );
        assert_eq!(
            drain_pipe(&mut reader, &mut output, &mut consumed, 8, deadline),
            Err("process-table-output")
        );
        assert_eq!(consumed, 9);
    }

    /// Stderr has an independent cumulative budget, so a noisy helper cannot
    /// hide behind repeated drain calls even when stdout stays below its cap.
    #[test]
    fn stderr_cap_accumulates_across_drains() {
        let mut reader = ScriptedReader {
            chunks: VecDeque::from([
                Ok(b"12345678".to_vec()),
                Err(io::ErrorKind::WouldBlock),
                Ok(b"9".to_vec()),
            ]),
        };
        let mut consumed = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            drain_pipe_discard(&mut reader, &mut consumed, 8, deadline),
            Ok(false)
        );
        assert_eq!(
            drain_pipe_discard(&mut reader, &mut consumed, 8, deadline),
            Err("process-table-output")
        );
        assert_eq!(consumed, 9);
    }
}
