// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Fixture setup and shared host-side helpers for the native Seatbelt probe.

/// Convert a non-UTF8-safe path without lossy Unicode replacement in the
/// worker argument vector.
fn path_arg(path: &Path) -> std::ffi::OsString {
    path.as_os_str().to_os_string()
}

/// Run every native security case under one private fixture root so failure
/// aggregation cannot skip diagnostics or leave a protected artifact behind.
pub fn run() -> Result<(), String> {
    require_seatbelt()?;
    let root = temporary_root()?;
    // The mode is passed to mkdir itself so another local user cannot read
    // the fixture during a create-then-chmod window.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .map_err(|error| error.to_string())?;
    if let Err(error) = fs::set_permissions(&root, fs::Permissions::from_mode(0o700)) {
        let _ = remove_tree(&root);
        return Err(error.to_string());
    }
    if let Err(error) = assert_private_mode(&root, 0o700) {
        let _ = remove_tree(&root);
        return Err(error);
    }
    let mut scope = match prepare_probe_scope(&root) {
        Ok(scope) => scope,
        Err(error) => {
            let _ = remove_tree(&root);
            return Err(error);
        }
    };
    let mut diagnostics = SandboxDenialDiagnostics::start();
    let result = run_all(&root, &mut diagnostics, &mut scope);
    let diagnostic_result = diagnostics.finish();
    let cleanup = remove_tree(&root);
    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error);
    }
    if let Err(error) = diagnostic_result {
        errors.push(error);
    }
    if let Err(error) = cleanup {
        errors.push(error);
    }
    if errors.is_empty() && (scope.active || !scope.entries.is_empty()) {
        errors.push("scope child evidence remained active".into());
    }
    if errors.is_empty() && env::var_os("CI").is_none() && env::var_os("RUNNER_TEMP").is_none() {
        if let Err(error) = remove_probe_scope(&scope) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

struct ProbeScope {
    path: PathBuf,
    root: String,
    entries: Vec<ScopeEntry>,
    active: bool,
    file_identity: Option<ScopeFileIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScopeFileIdentity {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
}

struct RegistrationFailure {
    identity: Option<ScopeIdentity>,
    category: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScopeEntry {
    pid: u32,
    pgid: i32,
    start_identity: String,
    comms: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScopeIdentity {
    pid: u32,
    pgid: i32,
    start_identity: String,
}

/// Private evidence that binds a setsid operation to its host parent before
/// the worker exists.  Keeping the provisional inode alive lets every later
/// report/read/query failure retain a recovery target instead of returning
/// with an escaped PID that the outer workflow cannot identify.
#[derive(Clone)]
struct EscapeEvidence {
    path: PathBuf,
    identity: ScopeFileIdentity,
    contents: Vec<u8>,
    operation_id: String,
    parent_pid: u32,
    parent_pgid: i32,
    nonce: u128,
    recovery_report: Option<PathBuf>,
}

/// Narrow escape-evidence filesystem boundary shared by the real cleanup and
/// injected tests; the same atomic transaction is exercised in both modes.
trait EscapeEvidenceIo {
    fn write_complete(&mut self, path: &Path, bytes: &[u8]) -> Result<ScopeFileIdentity, String>;
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String>;
    fn validate(&mut self, path: &Path, expected: ScopeFileIdentity) -> Result<(), String>;
    fn sync_parent(&mut self, path: &Path) -> Result<(), String>;
    fn unlink(&mut self, path: &Path) -> Result<(), String>;
}

struct RealEscapeEvidenceIo;

impl EscapeEvidenceIo for RealEscapeEvidenceIo {
    fn write_complete(&mut self, path: &Path, bytes: &[u8]) -> Result<ScopeFileIdentity, String> {
        real_write_escape_bytes(path, bytes)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        fs::rename(from, to).map_err(|_| "escape evidence restore publish failed".into())
    }

    fn validate(&mut self, path: &Path, expected: ScopeFileIdentity) -> Result<(), String> {
        validate_scope_path(path, Some(expected)).map(|_| ())
    }

    fn sync_parent(&mut self, path: &Path) -> Result<(), String> {
        sync_scope_directory(path)
    }

    fn unlink(&mut self, path: &Path) -> Result<(), String> {
        fs::remove_file(path).map_err(|_| "escape evidence remove failed".into())
    }
}

/// Create owner-only provisional escape evidence before a setsid worker is
/// spawned.  The unknown descendant fields are intentional: the evidence is
/// already durable before a report can be written or a PID can be observed.
fn prepare_escape_evidence(root: &Path, operation_id: &str) -> Result<EscapeEvidence, String> {
    if !valid_scope_atom(operation_id) {
        return Err("escape operation identity invalid".into());
    }
    // CI supplies a private evidence parent that outlives the disposable
    // fixture tree; local runs fall back to that tree because no outer
    // residual gate exists to own a separate temp root.
    let evidence_root = env::var_os("JA_SANDBOX_PRIVATE_ROOT")
        .or_else(|| env::var_os("RUNNER_TEMP"))
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_owned());
    let root_metadata =
        fs::symlink_metadata(&evidence_root).map_err(|_| "escape root stat failed")?;
    if !root_metadata.file_type().is_dir()
        || root_metadata.file_type().is_symlink()
        || root_metadata.permissions().mode() & 0o777 != 0o700
        || root_metadata.uid() != unsafe { geteuid() }
    {
        return Err("escape root is not private".into());
    }
    let parent_pid = std::process::id();
    let parent_pgid = unsafe { getpgrp() };
    if parent_pid <= 1 || parent_pgid <= 1 {
        return Err("escape parent identity is reserved".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "escape evidence clock failed")?
        .as_nanos();
    let path = evidence_root.join(format!(
        "ja-sandbox-escape-{operation_id}-{parent_pid}-{nonce}.evidence"
    ));
    let contents = escape_evidence_contents(
        operation_id,
        parent_pid,
        parent_pgid,
        nonce,
        "provisional",
        None,
        None,
    )?;
    let identity = write_escape_file(&path, &contents)?;
    sync_scope_directory(&path)?;
    Ok(EscapeEvidence {
        path,
        identity,
        contents: contents.into_bytes(),
        operation_id: operation_id.to_owned(),
        parent_pid,
        parent_pgid,
        nonce,
        recovery_report: None,
    })
}

/// Bind the fixture's private child-report path before spawn so a later host
/// failure can make one bounded recovery capture attempt without trusting a
/// path printed by the worker or exposing it in persistent evidence.
fn bind_escape_report(evidence: &mut EscapeEvidence, report: &Path) {
    evidence.recovery_report = Some(report.to_owned());
}

/// Atomically publish the descendant identity after a trusted report/query;
/// the provisional inode remains until the replacement is fully durable.
fn upgrade_escape_evidence(
    evidence: &mut EscapeEvidence,
    identity: Option<&ControlledProcessIdentity>,
    state: &'static str,
    failure: &'static str,
) -> Result<(), String> {
    let contents = escape_evidence_contents(
        &evidence.operation_id,
        evidence.parent_pid,
        evidence.parent_pgid,
        evidence.nonce,
        state,
        identity,
        Some(failure),
    )?;
    let pending = evidence.path.with_file_name(format!(
        ".{}-pending",
        evidence
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "escape evidence name invalid".to_string())?
    ));
    let pending_identity = write_escape_file(&pending, &contents)?;
    let result = (|| {
        validate_scope_path(&evidence.path, Some(evidence.identity))?;
        fs::rename(&pending, &evidence.path)
            .map_err(|_| "escape evidence publish failed".to_string())?;
        let identity = validate_scope_path(&evidence.path, None)?;
        sync_scope_directory(&evidence.path)?;
        Ok(identity)
    })();
    match result {
        Ok(identity) => {
            evidence.identity = identity;
            evidence.contents = contents.into_bytes();
            Ok(())
        }
        Err(error) => {
            // Before rename the pending inode is recovery evidence; after
            // rename the active inode carries the complete replacement.
            // Deleting either after a publication fault would leave the outer
            // cleanup without a trustworthy identity.
            let _ = pending_identity;
            eprintln!("SANDBOX-NATIVE: setsid pending evidence retained: {error}");
            Err(error)
        }
    }
}

/// Retain a fixed failure state when any setsid lifecycle step cannot be
/// trusted.  Failure evidence is never replaced by a success-looking image.
fn mark_escape_failure(
    evidence: &mut EscapeEvidence,
    identity: Option<&ControlledProcessIdentity>,
    category: &'static str,
) -> Result<(), String> {
    match upgrade_escape_evidence(evidence, identity, "failure", category) {
        Ok(()) => Ok(()),
        Err(error) => {
            let persistence = persist_escape_recovery_failure(evidence, category);
            flush_escape_evidence_before_abort(evidence);
            if persistence.is_err() {
                eprintln!("SANDBOX-NATIVE: setsid failure evidence persistence failed");
            }
            eprintln!("SANDBOX-NATIVE: setsid failure evidence unavailable: {error}");
            std::process::abort()
        }
    }
}

/// Remove escape evidence only after descriptor/path identity and directory
/// durability are both proven; a post-unlink sync fault restores the complete
/// image before the caller is allowed to fail closed.
fn remove_escape_evidence(
    io: &mut impl EscapeEvidenceIo,
    evidence: &EscapeEvidence,
) -> Result<(), String> {
    io.validate(&evidence.path, evidence.identity)?;
    io.unlink(&evidence.path)?;
    if io.sync_parent(&evidence.path).is_ok() {
        return Ok(());
    }
    match restore_escape_evidence(io, evidence) {
        Ok(()) => Err("escape evidence directory sync failed".into()),
        Err(error) => {
            let persistence = persist_escape_recovery_failure(evidence, "evidence-restore");
            flush_escape_evidence_before_abort(evidence);
            if persistence.is_err() {
                eprintln!("SANDBOX-NATIVE: escape failure evidence persistence failed");
            }
            eprintln!("SANDBOX-NATIVE: escape evidence restore unavailable: {error}");
            std::process::abort()
        }
    }
}

/// Recreate the complete evidence image through an owner-only same-directory
/// temporary inode; this is the recovery path after unlink succeeded but the
/// directory durability boundary failed.
fn restore_escape_evidence(
    io: &mut impl EscapeEvidenceIo,
    evidence: &EscapeEvidence,
) -> Result<(), String> {
    if evidence.contents.is_empty() {
        return Err("escape evidence image missing".into());
    }
    let pending = evidence.path.with_file_name(format!(
        ".{}-restore",
        evidence
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "escape evidence name invalid".to_string())?
    ));
    let pending_identity = io.write_complete(&pending, &evidence.contents)?;
    // On a pre-rename error the complete pending inode remains; after rename,
    // the active path itself contains the same complete image.  Either case
    // leaves an auditable recovery target for the outer gate.
    (|| {
        match fs::symlink_metadata(&evidence.path) {
            Ok(_) => return Err("escape evidence restore target occupied".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("escape evidence restore target stat failed".into()),
        }
        io.rename(&pending, &evidence.path)?;
        io.validate(&evidence.path, pending_identity)?;
        io.sync_parent(&evidence.path)
    })()
}

/// Persist a bounded, path-free upper-layer failure record before aborting if
/// the original evidence cannot be restored or upgraded.
fn persist_escape_recovery_failure(
    evidence: &EscapeEvidence,
    category: &'static str,
) -> Result<(), String> {
    if !valid_scope_atom(category) {
        return Err("escape recovery category invalid".into());
    }
    let path = evidence.path.with_file_name(format!(
        "{}.recovery-failure",
        evidence
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "escape evidence name invalid".to_string())?
    ));
    let contents = format!(
        "escape-recovery-version=1\noperation_id={}\nparent_pid={}\nparent_pgid={}\nnonce={}\nstate=failure\ncategory={category}\n",
        evidence.operation_id, evidence.parent_pid, evidence.parent_pgid, evidence.nonce
    );
    let _ = write_escape_file(&path, &contents)?;
    sync_scope_directory(&path)
}

/// Flush the private evidence directory and diagnostic streams before an
/// intentional abort; this keeps the fixed failure marker observable even
/// when the restore transaction itself has become uncertain.
fn flush_escape_evidence_before_abort(evidence: &EscapeEvidence) {
    let _ = sync_scope_directory(&evidence.path);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

/// Read only a small fixed report image through a no-follow descriptor; a
/// same-user replacement or an oversized worker report must enter the common
/// failure finalizer instead of allocating unbounded host memory.
fn read_bounded_report(path: &Path) -> Result<String, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(0x0000_0100 | 0x0100_0000)
        .open(path)
        .map_err(|_| "setsid report open failed".to_string())?;
    let mut bytes = Vec::new();
    let read = Read::by_ref(&mut file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "setsid report read failed".to_string())?;
    if read > 4096 {
        return Err("setsid report overflow".into());
    }
    String::from_utf8(bytes).map_err(|_| "setsid report encoding failed".into())
}

/// Build a bounded, fixed-field evidence image without paths, secrets or raw
/// process output.  Unknown descendant fields remain explicit until capture.
fn escape_evidence_contents(
    operation_id: &str,
    parent_pid: u32,
    parent_pgid: i32,
    nonce: u128,
    state: &str,
    identity: Option<&ControlledProcessIdentity>,
    failure: Option<&str>,
) -> Result<String, String> {
    if !valid_scope_atom(operation_id)
        || !valid_scope_atom(state)
        || parent_pid <= 1
        || parent_pgid <= 1
        || nonce == 0
    {
        return Err("escape evidence identity invalid".into());
    }
    let (pid, pgid, start_identity, comm) = match identity {
        Some(identity) if identity.pid > 1 && identity.pgid > 1 => (
            identity.pid.to_string(),
            identity.pgid.to_string(),
            identity.start_identity.clone(),
            identity.comm.clone(),
        ),
        Some(_) => return Err("escape descendant identity reserved".into()),
        None => (
            "unknown".into(),
            "unknown".into(),
            "unknown".into(),
            "unknown".into(),
        ),
    };
    if start_identity != "unknown" {
        let checked = ControlledProcessIdentity {
            pid: pid.parse().map_err(|_| "escape pid invalid")?,
            pgid: pgid.parse().map_err(|_| "escape pgid invalid")?,
            start_identity,
            comm,
        };
        if !valid_scope_identity(&checked) {
            return Err("escape descendant identity invalid".into());
        }
        let failure = failure.unwrap_or("none");
        if !valid_scope_atom(failure) {
            return Err("escape failure category invalid".into());
        }
        return Ok(format!(
            "escape-version=1\noperation_id={operation_id}\nparent_pid={parent_pid}\nparent_pgid={parent_pgid}\nnonce={nonce}\nstate={state}\ndescendant_pid={pid}\ndescendant_pgid={pgid}\ndescendant_start_identity={}\ndescendant_comm={}\nfailure={failure}\n",
            checked.start_identity, checked.comm
        ));
    }
    let failure = failure.unwrap_or("none");
    if !valid_scope_atom(failure) {
        return Err("escape failure category invalid".into());
    }
    Ok(format!(
        "escape-version=1\noperation_id={operation_id}\nparent_pid={parent_pid}\nparent_pgid={parent_pgid}\nnonce={nonce}\nstate={state}\ndescendant_pid=unknown\ndescendant_pgid=unknown\ndescendant_start_identity=unknown\ndescendant_comm=unknown\nfailure={failure}\n"
    ))
}

/// Create and fsync one owner-only evidence image with no chmod visibility
/// window; the descriptor identity is returned for all later unlink checks.
fn write_escape_file(path: &Path, contents: &str) -> Result<ScopeFileIdentity, String> {
    write_escape_bytes(path, contents.as_bytes())
}

/// Publish one bounded owner-only byte image through a no-follow descriptor;
/// callers use the returned identity to detect path or inode replacement.
fn write_escape_bytes(path: &Path, contents: &[u8]) -> Result<ScopeFileIdentity, String> {
    let mut io = RealEscapeEvidenceIo;
    io.write_complete(path, contents)
}

/// Perform the real owner-only byte write used by `RealEscapeEvidenceIo`;
/// fault plans wrap this operation instead of copying its security checks.
fn real_write_escape_bytes(path: &Path, contents: &[u8]) -> Result<ScopeFileIdentity, String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0x0000_0100 | 0x0100_0000)
        .open(path)
        .map_err(|_| "escape evidence create failed".to_string())?;
    let initial = validate_scope_file(&file, None)?;
    file.write_all(contents)
        .map_err(|_| "escape evidence write failed".to_string())?;
    file.sync_all()
        .map_err(|_| "escape evidence sync failed".to_string())?;
    validate_scope_file(&file, Some(initial))
}

impl ProbeScope {
    /// Register the child identity before its lifecycle case can finish so a
    /// later process-table pass can distinguish PID reuse from a real leak.
    fn register_child(
        &mut self,
        child: &SandboxChild,
        expected_comm: &str,
    ) -> Result<ScopeIdentity, RegistrationFailure> {
        let pid = child.process_id();
        let pgid = child.process_group_id();
        if pid <= 1 || pgid <= 1 {
            return Err(RegistrationFailure {
                identity: None,
                category: "scope child identity is reserved".into(),
            });
        }
        if !valid_scope_atom(expected_comm) {
            return Err(RegistrationFailure {
                identity: None,
                category: "scope child command identity invalid".into(),
            });
        }
        if self.entries.iter().any(|entry| entry.pid == pid) {
            return Err(RegistrationFailure {
                identity: None,
                category: "scope child PID identity changed".into(),
            });
        }
        let provisional = ScopeIdentity {
            pid,
            pgid,
            start_identity: "provisional".into(),
        };
        let mut provisional_entry = ScopeEntry {
            pid,
            pgid,
            start_identity: provisional.start_identity.clone(),
            comms: vec![expected_comm.to_owned()],
        };
        let mut candidate = self.entries.clone();
        candidate.push(provisional_entry.clone());
        if let Err(error) = self.commit_with_restore(&candidate) {
            return Err(RegistrationFailure {
                identity: None,
                category: error,
            });
        }
        self.entries = candidate;
        self.active = true;

        let identity = match query_controlled_identity(pid) {
            Ok(identity) => identity,
            Err(_) => {
                return Err(RegistrationFailure {
                    identity: Some(provisional),
                    category: "scope child identity query failed".into(),
                });
            }
        };
        if identity.pgid != pgid || !valid_scope_identity(&identity) {
            return Err(RegistrationFailure {
                identity: Some(provisional),
                category: "scope child identity mismatch".into(),
            });
        }
        let mut comms = vec![identity.comm.clone()];
        if expected_comm != identity.comm {
            comms.push(expected_comm.to_owned());
        }
        provisional_entry.start_identity = identity.start_identity.clone();
        provisional_entry.comms = comms;
        let mut upgraded = self.entries.clone();
        upgraded.retain(|entry| entry.pid != pid);
        upgraded.push(provisional_entry);
        if let Err(error) = self.commit_with_restore(&upgraded) {
            return Err(RegistrationFailure {
                identity: Some(provisional),
                category: error,
            });
        }
        self.entries = upgraded;
        Ok(ScopeIdentity {
            pid,
            pgid,
            start_identity: identity.start_identity,
        })
    }

    /// Remove one completed child only after the replacement evidence is
    /// durable; a failed commit leaves the old identity in memory and on disk.
    fn unregister_child(&mut self, identity: &ScopeIdentity) -> Result<(), String> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.pid == identity.pid)
            .ok_or_else(|| "scope child identity missing".to_string())?;
        let entry = &self.entries[index];
        if entry.pgid != identity.pgid || entry.start_identity != identity.start_identity {
            return Err("scope child identity changed".into());
        }
        let mut candidate = self.entries.clone();
        candidate.remove(index);
        self.commit_with_restore(&candidate)?;
        if candidate.is_empty() {
            if let Err(error) = remove_scope_file(&self.path, self.file_identity) {
                return self.restore_after_failure(error);
            }
            self.active = false;
            self.file_identity = None;
        }
        self.entries = candidate;
        Ok(())
    }

    /// Atomically rewrite all entries, preserving the prior evidence if the
    /// filesystem reports an error after rename or directory synchronization.
    fn commit_candidate(&mut self, candidate: &[ScopeEntry]) -> Result<(), String> {
        if self.active {
            validate_scope_path(&self.path, self.file_identity)?;
        }
        let pending = pending_scope_path(&self.path)?;
        let pending_identity = write_scope_file(&pending, &self.root, candidate)?;
        let result = fs::rename(&pending, &self.path)
            .map_err(|_| "scope evidence rename failed".to_string())
            .and_then(|_| {
                let identity = validate_scope_path(&self.path, None)?;
                self.file_identity = Some(identity);
                sync_scope_directory(&self.path)
            });
        if result.is_err() {
            let _ = remove_scope_path_if_identity(&pending, pending_identity);
        }
        result
    }

    /// Recreate the owner-only scope inode when the previous child-level
    /// unregister removed the empty scope; later workers still get their own
    /// active evidence image rather than relying on a final global cleanup.
    #[cfg(test)]
    fn ensure_active_scope(&mut self) -> Result<(), String> {
        if self.active {
            return Ok(());
        }
        let identity = write_scope_file(&self.path, &self.root, &[])?;
        sync_scope_directory(&self.path)?;
        self.file_identity = Some(identity);
        self.active = true;
        Ok(())
    }

    /// Restore the last known-good identity image when a filesystem error is
    /// reported after a candidate publication, so unregister failure cannot
    /// silently erase the only proof of an active child.
    fn commit_with_restore(&mut self, candidate: &[ScopeEntry]) -> Result<(), String> {
        match self.commit_candidate(candidate) {
            Ok(()) => Ok(()),
            Err(error) => self.restore_after_failure(error),
        }
    }

    /// Re-publish the current entries after a failed mutation; if restoration
    /// also fails, the fixed error remains fail-closed and the gate must fail.
    fn restore_after_failure(&mut self, error: String) -> Result<(), String> {
        let restored = if self.path.exists() {
            if validate_scope_path(&self.path, self.file_identity).is_err() {
                Err("scope evidence identity changed".into())
            } else {
                let entries = self.entries.clone();
                self.commit_candidate(&entries)
            }
        } else {
            write_scope_file(&self.path, &self.root, &self.entries).and_then(|identity| {
                self.file_identity = Some(identity);
                sync_scope_directory(&self.path)
            })
        };
        match restored {
            Ok(()) => Err(error),
            Err(_restore_error) => Err("scope evidence restore failed".into()),
        }
    }
}

/// Validate the existing scope inode immediately before an atomic replacement
/// so a same-user symlink/hardlink swap cannot redirect the evidence path.
fn validate_scope_path(
    path: &Path,
    expected: Option<ScopeFileIdentity>,
) -> Result<ScopeFileIdentity, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x0000_0100 | 0x0100_0000)
        .open(path)
        .map_err(|_| "scope evidence open failed".to_string())?;
    let descriptor_identity = validate_scope_file(&file, expected)?;
    // fstat alone does not prove that the pathname still names the opened
    // inode.  The immediate lstat comparison closes the observable swap
    // window as far as this same-process cleanup contract can honestly do.
    let path_metadata =
        fs::symlink_metadata(path).map_err(|_| "scope evidence path stat failed".to_string())?;
    let path_identity = ScopeFileIdentity {
        dev: path_metadata.dev(),
        ino: path_metadata.ino(),
        nlink: path_metadata.nlink(),
        mode: path_metadata.permissions().mode() & 0o777,
        uid: path_metadata.uid(),
    };
    if path_metadata.file_type().is_symlink() || path_identity != descriptor_identity {
        return Err("scope evidence path identity changed".into());
    }
    Ok(descriptor_identity)
}

/// Bind every active or pending scope check to the opened descriptor so a
/// same-user path replacement cannot turn a 0600 claim into a different inode.
fn validate_scope_file(
    file: &File,
    expected: Option<ScopeFileIdentity>,
) -> Result<ScopeFileIdentity, String> {
    let metadata = file
        .metadata()
        .map_err(|_| "scope evidence stat failed".to_string())?;
    let identity = ScopeFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    if !metadata.file_type().is_file()
        || identity.uid != unsafe { geteuid() }
        || identity.mode != 0o600
        || identity.nlink != 1
        || expected.is_some_and(|expected| expected != identity)
    {
        return Err("scope evidence inode changed".into());
    }
    Ok(identity)
}

/// Create a unique pending sibling with owner-only mode before writing any
/// replacement bytes; the final rename is the only publication point.
fn pending_scope_path(path: &Path) -> Result<PathBuf, String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "scope evidence nonce failed".to_string())?
        .as_nanos();
    Ok(path.with_file_name(format!(
        "{}.pending-{}-{nonce}",
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "scope evidence name invalid".to_string())?,
        std::process::id()
    )))
}

/// Write and fsync one complete evidence image before it can replace the
/// active scope, keeping partial identity lists out of the residual scanner.
fn write_scope_file(
    path: &Path,
    root: &str,
    entries: &[ScopeEntry],
) -> Result<ScopeFileIdentity, String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0x0000_0100 | 0x0100_0000)
        .open(path)
        .map_err(|_| "scope evidence create failed".to_string())?;
    let initial_identity = match validate_scope_file(&file, None) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(error);
        }
    };
    if let Err(error) = validate_scope_file(&file, Some(initial_identity)) {
        let _ = remove_scope_path_if_identity(path, initial_identity);
        return Err(error);
    }
    let result = file
        .write_all(b"scope-version=1\n")
        .and_then(|_| file.write_all(format!("root={root}\n").as_bytes()))
        .and_then(|_| {
            for entry in entries {
                let line = format!(
                    "entry\tpid={}\tpgid={}\tstart_identity={}\tcomm={}\n",
                    entry.pid,
                    entry.pgid,
                    entry.start_identity,
                    entry.comms.join("|")
                );
                file.write_all(line.as_bytes())?;
            }
            Ok(())
        })
        .and_then(|_| file.sync_all());
    if let Err(error) = result {
        let _ = remove_scope_path_if_identity(path, initial_identity);
        return Err(format!("scope evidence write failed: {error}"));
    }
    let identity = match validate_scope_file(&file, None) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = remove_scope_path_if_identity(path, initial_identity);
            return Err(error);
        }
    };
    Ok(identity)
}

/// Sync the parent directory after create/rename/remove so the active scope
/// publication and child-level unregister survive a runner crash boundary.
fn sync_scope_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "scope evidence parent missing".to_string())?;
    File::open(parent)
        .map_err(|_| "scope evidence directory open failed".to_string())?
        .sync_all()
        .map_err(|_| "scope evidence directory sync failed".to_string())
}

/// Remove one inactive scope inode only after its directory entry is durable;
/// callers retain the identity if either operation cannot be proven.
fn remove_scope_file(path: &Path, expected: Option<ScopeFileIdentity>) -> Result<(), String> {
    validate_scope_path(path, expected)?;
    fs::remove_file(path).map_err(|_| "scope evidence remove failed".to_string())?;
    sync_scope_directory(path)
}

/// Unlink a scope path only while its descriptor-bound identity still equals
/// the image created by this writer; cleanup failures remain fail-closed.
fn remove_scope_path_if_identity(path: &Path, expected: ScopeFileIdentity) -> Result<(), String> {
    validate_scope_path(path, Some(expected))?;
    fs::remove_file(path).map_err(|_| "scope evidence remove failed".to_string())?;
    sync_scope_directory(path)
}

/// Keep process identities field-safe so scope parsing rejects delimiters,
/// newlines and Unicode look-alikes before they reach the evidence file.
fn valid_scope_identity(identity: &ControlledProcessIdentity) -> bool {
    valid_scope_start(&identity.start_identity) && valid_scope_atom(&identity.comm)
}

/// Accept only the printable ASCII `lstart` grammar so delimiter injection or
/// Unicode look-alikes cannot alter a later process-table identity comparison.
fn valid_scope_start(value: &str) -> bool {
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

/// Restrict command names to a stable ASCII atom while allowing the opaque
/// command arguments to retain the fixture's Unicode workspace path.
fn valid_scope_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._/-".contains(character))
}

/// Prepare the exact temporary-root identity before any worker spawn; the
/// first child publishes its provisional scope only after its PID is known.
fn prepare_probe_scope(root: &Path) -> Result<ProbeScope, String> {
    let evidence_root = env::var_os("JA_SANDBOX_PRIVATE_ROOT")
        .or_else(|| env::var_os("RUNNER_TEMP"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    if !evidence_root.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&evidence_root)
            .map_err(|error| error.to_string())?;
    }
    let metadata = fs::symlink_metadata(&evidence_root).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != unsafe { geteuid() }
    {
        return Err("probe scope directory is not owner-private".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = evidence_root.join(format!(
        "ja-sandbox-probe-scope-{}-{nonce}.scope",
        std::process::id()
    ));
    let root = root
        .to_str()
        .filter(|value| !value.is_empty() && !value.contains('\n') && !value.contains('\r'))
        .ok_or_else(|| "probe scope root is not representable".to_string())?;
    Ok(ProbeScope {
        path,
        root: root.to_owned(),
        entries: Vec::new(),
        // No empty scope is published before a child exists.  The first
        // post-spawn operation atomically publishes its provisional identity.
        active: false,
        file_identity: None,
    })
}

/// Remove local scope evidence only after a successful probe; CI retains it so
/// the independent process-table gate can prove no worker survived success.
fn remove_probe_scope(scope: &ProbeScope) -> Result<(), String> {
    if scope.active || !scope.entries.is_empty() {
        return Err("probe scope still has active child evidence".into());
    }
    Ok(())
}

/// Probe the real Seatbelt executable and reject a missing/blocked host
/// before creating any fixture, avoiding an accidental unsandboxed pass.
fn require_seatbelt() -> Result<(), String> {
    let executable = Path::new("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        return Err("/usr/bin/sandbox-exec is unavailable".into());
    }
    let mut command = Command::new(executable);
    command.args(["-p", "(version 1) (allow default)", "/usr/bin/true"]);
    let output = run_bounded_command(command, Duration::from_secs(2), 64 * 1024, 64 * 1024)
        .map_err(|error| format!("sandbox-exec capability probe failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sandbox-exec capability probe rejected with code {:?}",
            output.status.code()
        ));
    }
    Ok(())
}

/// Run positive/negative access cases and all process-lifecycle hazards in
/// one temporary tree so cleanup and security snapshots share a boundary.
fn run_all(
    root: &Path,
    diagnostics: &mut SandboxDenialDiagnostics,
    scope: &mut ProbeScope,
) -> Result<(), String> {
    let workspace = root.join("workspace 中文 空格");
    let outside_dir = root.join("outside");
    let resource_dir = root.join("resource");
    fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    fs::create_dir_all(&outside_dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(&resource_dir).map_err(|error| error.to_string())?;
    let worker_source = worker_binary()?;
    let worker = resource_dir.join("ja-sandbox-worker");
    fs::copy(&worker_source, &worker).map_err(|error| error.to_string())?;
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755))
        .map_err(|error| error.to_string())?;
    let resource_sibling = resource_dir.join("sibling.txt");
    fs::write(
        &resource_sibling,
        b"resource sibling must remain unreadable",
    )
    .map_err(|error| error.to_string())?;
    let allowed = workspace.join("allowed.txt");
    fs::write(&allowed, b"allowed workspace content").map_err(|error| error.to_string())?;
    let workspace_exe = workspace.join("workspace-executable");
    fs::copy(&worker_source, &workspace_exe).map_err(|error| error.to_string())?;
    fs::set_permissions(&workspace_exe, fs::Permissions::from_mode(0o755))
        .map_err(|error| error.to_string())?;
    let outside = outside_dir.join("outside.txt");
    let secret = outside_dir.join("provider-secret.txt");
    let product_db = outside_dir.join("ja.sqlite");
    fs::write(&outside, b"outside content").map_err(|error| error.to_string())?;
    fs::write(&secret, b"ja-fixture-protected-content").map_err(|error| error.to_string())?;
    fs::write(&product_db, b"SQLite format 3\0").map_err(|error| error.to_string())?;
    let escape_link = workspace.join("escape-link");
    let dotdot = workspace.join("..").join("outside").join("outside.txt");
    let loopback = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let loopback_address = loopback.local_addr().map_err(|error| error.to_string())?;
    let external_address = "1.1.1.1:80";
    run_case(
        "hardlink-preflight",
        run_hardlink_case(root, &workspace, &worker, &secret, diagnostics, scope),
        diagnostics,
    )?;
    symlink(&secret, &escape_link)
        .map_err(|error| format!("symlink fixture unavailable: {error}"))?;
    let baseline = snapshot(&[
        &workspace,
        &allowed,
        &resource_dir,
        &worker,
        &resource_sibling,
        &outside,
        &secret,
        &product_db,
    ])?;

    let smoke_report = run_smoke(
        root,
        &workspace,
        &worker,
        &outside,
        &dotdot,
        &escape_link,
        &secret,
        &product_db,
        &workspace_exe,
        &resource_sibling,
        loopback_address,
        external_address,
        scope,
    )?;
    assert_smoke(&smoke_report)?;
    diagnostics.pump();
    println!("SANDBOX-CASE-PASS: smoke");
    let after_smoke = snapshot(&[
        &workspace,
        &allowed,
        &resource_dir,
        &worker,
        &resource_sibling,
        &outside,
        &secret,
        &product_db,
    ])?;
    if baseline != after_smoke {
        return Err("security attributes/content changed during smoke".into());
    }
    run_case(
        "timeout",
        run_timeout_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    run_case(
        "parent-exit",
        run_parent_exit_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    run_case(
        "overflow",
        run_overflow_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    run_case(
        "cancel",
        run_cancel_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    run_case(
        "setsid",
        run_setsid_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    run_case(
        "setsid-continuous-output",
        run_setsid_output_case(root, &workspace, &worker, scope),
        diagnostics,
    )?;
    println!(
        "SANDBOX-PASS: seatbelt, paths, environment, network, output and process-tree cleanup"
    );
    Ok(())
}

/// Build a worker spec with a unique profile and no inherited parent env.
#[allow(clippy::too_many_arguments)]
fn child_for<'scope>(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    label: &str,
    mode: &str,
    report: &Path,
    child_report: &Path,
    release: &Path,
    continuous: bool,
    scope: &'scope mut ProbeScope,
) -> Result<ScopedNativeChild<'scope>, String> {
    let profile = root.join(format!("profile-{label}.sb"));
    let mut spec = SandboxSpec::new(worker, workspace, profile);
    spec.args = argv![
        "--mode",
        mode,
        "--report",
        path_arg(report),
        "--child-report",
        path_arg(child_report),
        "--release",
        path_arg(release),
    ];
    if continuous {
        spec.args.push("--continuous".into());
    }
    spec.env = baseline_env();
    spawn_registered(scope, spec, "ja-sandbox-worker").map_err(|error| error.to_string())
}

/// Keep the production worker spawn and identity registration inseparable so
/// every native child enters the durable scope before any probe operation.
fn spawn_registered<'scope>(
    scope: &'scope mut ProbeScope,
    spec: SandboxSpec,
    expected_comm: &str,
) -> Result<ScopedNativeChild<'scope>, SandboxError> {
    let child = spawn(spec)?;
    register_spawned(scope, child, expected_comm)
        .map_err(|_| SandboxError::ChildCleanup("scope registration failed"))
}

/// Retain the spawned child while identity registration runs; a failed query
/// must still cancel and reap the exact process instead of releasing it to
/// `Drop` with an unknown PID/PGID.
fn register_spawned<'scope>(
    scope: &'scope mut ProbeScope,
    mut child: SandboxChild,
    expected_comm: &str,
) -> Result<ScopedNativeChild<'scope>, String> {
    let identity = match scope.register_child(&child, expected_comm) {
        Ok(identity) => identity,
        Err(failure) => {
            let _ = child.cancel();
            let _ = child.wait_with_output(Duration::from_secs(2));
            if !child.cleanup_confirmed() {
                fail_closed_child_cleanup(scope, failure.identity.as_ref());
            }
            if let Some(identity) = failure.identity.as_ref()
                && scope.unregister_child(identity).is_err()
            {
                fail_closed_child_cleanup(scope, Some(identity));
            }
            return Err(failure.category);
        }
    };
    Ok(ScopedNativeChild {
        child: Some(child),
        scope,
        identity,
        cleanup_proven: false,
        unregistered: false,
    })
}

/// Own one registered worker until its direct child and original process
/// group are proven gone; scope removal is coupled to this lifecycle owner.
struct ScopedNativeChild<'scope> {
    child: Option<SandboxChild>,
    scope: &'scope mut ProbeScope,
    identity: ScopeIdentity,
    cleanup_proven: bool,
    unregistered: bool,
}

impl ScopedNativeChild<'_> {
    /// Delegate cancellation without unregistering early; the subsequent
    /// bounded wait must prove reaping before durable evidence is removed.
    fn cancel(&mut self) -> Result<(), SandboxError> {
        self.child
            .as_mut()
            .ok_or(SandboxError::ChildCleanup("child ownership missing"))?
            .cancel()
    }

    /// Poll only the owned direct child; a terminal status still goes through
    /// `wait_with_output` so pipes, group and scope evidence close together.
    fn poll_status(&mut self) -> Result<Option<std::process::ExitStatus>, SandboxError> {
        self.child
            .as_mut()
            .ok_or(SandboxError::ChildCleanup("child ownership missing"))?
            .poll_status()
    }

    /// Wait, prove full cleanup, then drop the child and unregister its exact
    /// identity.  Any unproven cleanup or durable-write failure stays fatal.
    fn wait_with_output(&mut self, timeout: Duration) -> Result<RunOutput, SandboxError> {
        let result = self
            .child
            .as_mut()
            .ok_or(SandboxError::ChildCleanup("child ownership missing"))?
            .wait_with_output(timeout);
        let proven = self
            .child
            .as_ref()
            .is_some_and(SandboxChild::cleanup_confirmed);
        if !proven {
            return Err(SandboxError::ChildCleanup(
                "scope child cleanup unconfirmed",
            ));
        }
        let child = self
            .child
            .take()
            .ok_or(SandboxError::ChildCleanup("child ownership missing"))?;
        drop(child);
        self.cleanup_proven = true;
        if self.scope.unregister_child(&self.identity).is_err() {
            return Err(SandboxError::ChildCleanup(
                "scope evidence unregister failed",
            ));
        }
        self.unregistered = true;
        result
    }
}

impl Drop for ScopedNativeChild<'_> {
    /// Drop is a final bounded cleanup attempt; evidence is removed only when
    /// the original PID and PGID are observably gone, otherwise the residual
    /// scanner receives the still-active scope identity.
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let proven_before_drop = child.cleanup_confirmed();
            let pid = i32::try_from(self.identity.pid).ok();
            let pgid = self.identity.pgid;
            if !proven_before_drop {
                let _ = persist_scope_failure(
                    self.scope,
                    Some(&self.identity),
                    "child-cleanup-unconfirmed",
                );
            }
            drop(child);
            self.cleanup_proven = proven_before_drop
                || pid.is_some_and(|pid| {
                    matches!(process_is_alive(pid), Ok(false))
                        && matches!(process_group_is_gone(pgid), Ok(true))
                });
        }
        if self.cleanup_proven && !self.unregistered {
            if self.scope.unregister_child(&self.identity).is_ok() {
                self.unregistered = true;
            } else {
                let _ = persist_scope_failure(
                    self.scope,
                    Some(&self.identity),
                    "scope-unregister-failed",
                );
                eprintln!("SANDBOX-NATIVE-CHILD: scope-unregister-failed");
                std::process::abort();
            }
        }
    }
}

/// Convert an untracked registration cleanup failure into a fixed process
/// abort; returning would let a live child escape without durable identity.
fn fail_closed_child_cleanup(scope: &ProbeScope, identity: Option<&ScopeIdentity>) -> ! {
    let _ = persist_scope_failure(scope, identity, "registration-cleanup-unconfirmed");
    eprintln!("SANDBOX-NATIVE-CHILD: registration-cleanup-unconfirmed");
    std::process::abort()
}

/// Persist a fixed failure marker before aborting so an outer residual gate
/// can distinguish cleanup failure from a clean run even when scope rewriting
/// itself is unavailable.  The marker is owner-only and contains no path or
/// secret, only the already validated identity and fixed category.
fn persist_scope_failure(
    scope: &ProbeScope,
    identity: Option<&ScopeIdentity>,
    category: &'static str,
) -> Result<(), String> {
    let name = scope
        .path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "scope failure marker name invalid".to_string())?;
    let path = scope.path.with_file_name(format!("{name}.failure"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0x0000_0100 | 0x0100_0000)
        .open(&path)
        .map_err(|_| "scope failure marker create failed".to_string())?;
    let identity_fields = identity
        .filter(|value| value.pid > 1 && value.pgid > 1)
        .map(|value| format!("pid={}\npgid={}\n", value.pid, value.pgid))
        .unwrap_or_else(|| "pid=unavailable\npgid=unavailable\n".into());
    file.write_all(format!("scope-failure={category}\n{identity_fields}").as_bytes())
        .map_err(|_| "scope failure marker write failed".to_string())?;
    file.sync_all()
        .map_err(|_| "scope failure marker sync failed".to_string())?;
    validate_scope_file(&file, None)?;
    sync_scope_directory(&path)
}

/// Verify report PID and wait until the kernel no longer exposes it.
fn verify_descendant_gone(report: &Path) -> Result<(), String> {
    wait_for_file(report, Duration::from_secs(2))?;
    let contents = fs::read_to_string(report).map_err(|error| error.to_string())?;
    let pid = parse_report_pid(&contents, "descendant report omitted pid")?;
    verify_process_gone(pid)
}

/// The setsid worker emits one of two complete records; keeping the state
/// explicit prevents a denial marker and a claimed PID from being accepted as
/// the same lifecycle.  No caller may treat an invalid record as “no child”.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetsidReport {
    Denied,
    Descendant { pid: i32 },
}

/// Parse the complete setsid report grammar before any liveness query or
/// direct signal can be attempted. Exact keys, values, and state combinations
/// are required so an appended or duplicated line cannot hide a live child.
fn parse_setsid_report(contents: &str) -> Result<SetsidReport, String> {
    if contents.is_empty() || contents.contains('\r') || contents.ends_with('\n') {
        return Err("setsid report grammar invalid".into());
    }
    let mut started = None;
    let mut denied = None;
    let mut pid = None;
    for line in contents.split('\n') {
        if line.is_empty() {
            return Err("setsid report grammar invalid".into());
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "setsid report grammar invalid".to_string())?;
        if key.is_empty() || value.is_empty() || value.contains('=') {
            return Err("setsid report grammar invalid".into());
        }
        match key {
            "setsid-started" => {
                if started.is_some() || value != "true" {
                    return Err("setsid report grammar invalid".into());
                }
                started = Some(true);
            }
            "setsid-denied" => {
                if denied.is_some() || value != "true" {
                    return Err("setsid report grammar invalid".into());
                }
                denied = Some(true);
            }
            "pid" => {
                if pid.is_some() {
                    return Err("setsid report grammar invalid".into());
                }
                let value = parse_strict_pid(value)
                    .ok_or_else(|| "setsid report grammar invalid".to_string())?;
                pid = Some(value);
            }
            _ => return Err("setsid report grammar invalid".into()),
        }
    }
    match (started, denied, pid) {
        (None, Some(true), None) => Ok(SetsidReport::Denied),
        (Some(true), None, Some(pid)) => Ok(SetsidReport::Descendant { pid }),
        _ => Err("setsid report grammar invalid".into()),
    }
}

/// Parse generic fixture PID evidence while rejecting reserved values before
/// any liveness query or direct signal can be attempted. The complete record
/// may contain one known marker plus exactly one PID field.
fn parse_report_pid(contents: &str, missing: &'static str) -> Result<i32, String> {
    let mut pid = None;
    let mut marker = None;
    for line in contents.split('\n') {
        let (key, value) = line.split_once('=').ok_or_else(|| missing.to_string())?;
        if key.is_empty() || value.is_empty() || value.contains('=') {
            return Err(missing.to_string());
        }
        if key == "pid" {
            if pid.is_some() {
                return Err(missing.to_string());
            }
            pid = Some(parse_strict_pid(value).ok_or_else(|| missing.to_string())?);
        } else if matches!(key, "grandchild-started" | "idle" | "idle-output")
            && value == "true"
            && marker.replace(key).is_none()
        {
            continue;
        } else {
            return Err(missing.to_string());
        }
    }
    match (marker, pid) {
        (Some(_), Some(pid)) => Ok(pid),
        _ => Err(missing.to_string()),
    }
}

/// Accept only an unsigned decimal PID above the reserved boundary; signs,
/// whitespace, and Unicode look-alikes are rejected before any OS query.
fn parse_strict_pid(value: &str) -> Option<i32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<i32>().ok().filter(|value| *value > 1)
}

/// Confirm an escaped descendant disappears after the probe's bounded cleanup;
/// returning immediately would let a setsid child outlive the failed gate.
fn verify_process_gone(pid: i32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match process_is_alive(pid) {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(_) => return Err("descendant identity query failed".into()),
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err("descendant survived process-group cleanup".into())
}

/// Use only the minimum runtime variables needed by the worker and omit
/// PATH plus all parent/provider secrets.
fn baseline_env() -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    let mut env = BTreeMap::new();
    for key in ["HOME", "USER", "LOGNAME", "SHELL", "TERM", "TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            env.insert(key.into(), value);
        }
    }
    env
}

/// Snapshot mode, content hash and xattrs through host utilities before
/// and after the worker, so a passing access test also proves no protected
/// fixture mutation occurred.
fn snapshot(paths: &[&Path]) -> Result<Vec<String>, String> {
    paths
        .iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
            let mut xattr_command = Command::new("/usr/bin/xattr");
            xattr_command.arg("-l").arg(path);
            let xattrs =
                run_bounded_command(xattr_command, Duration::from_secs(2), 64 * 1024, 64 * 1024)
                    .map_err(|error| error.to_string())?;
            if !xattrs.status.success() {
                return Err("xattr inspection failed".into());
            }
            let mut acl_command = Command::new("/bin/ls");
            acl_command.args(["-lde"]).arg(path);
            let acl =
                run_bounded_command(acl_command, Duration::from_secs(2), 64 * 1024, 64 * 1024)
                    .map_err(|error| error.to_string())?;
            if !acl.status.success() {
                return Err("ACL inspection failed".into());
            }
            let digest = if metadata.is_file() {
                let mut hash_command = Command::new("/usr/bin/shasum");
                hash_command.args(["-a", "256"]).arg(path);
                let hash =
                    run_bounded_command(hash_command, Duration::from_secs(2), 64 * 1024, 64 * 1024)
                        .map_err(|error| error.to_string())?;
                if !hash.status.success() {
                    return Err("content hash inspection failed".into());
                }
                String::from_utf8_lossy(&hash.stdout)
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| "content hash output was empty".to_string())?
                    .to_string()
            } else {
                "directory".to_string()
            };
            Ok(format!(
                "mode={:o};size={};hash={};xattr={};acl={}",
                metadata.permissions().mode(),
                metadata.len(),
                digest,
                String::from_utf8_lossy(&xattrs.stdout),
                String::from_utf8_lossy(&acl.stdout)
            ))
        })
        .collect()
}

/// Confirm the probe's temporary root and generated policy never expose
/// fixture paths to another local user while a worker is active.
fn assert_private_mode(path: &Path, expected: u32) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != expected {
        return Err(format!(
            "private fixture mode mismatch: expected {expected:o}, got {mode:o}"
        ));
    }
    Ok(())
}

/// Locate the sibling fixture emitted by Cargo, refusing a system or
/// user-provided replacement executable.
fn worker_binary() -> Result<PathBuf, String> {
    let mut path = env::current_exe().map_err(|error| error.to_string())?;
    path.set_file_name("ja-sandbox-worker");
    if path.is_file() {
        Ok(path)
    } else {
        Err("ja-sandbox-worker is not next to the probe".into())
    }
}

/// Wait on a fixture file rather than sleeping a guessed duration.
fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err("fixture report barrier timed out".into())
}

/// Create a collision-resistant Unicode/space path and remove only this
/// exact tree after all children and profiles have been closed.
fn temporary_root() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    Ok(env::temp_dir().join(format!(
        "JA sandbox 中文 空格 {} {nanos}",
        std::process::id()
    )))
}

/// Remove the exact fixture tree; cleanup failure is a hard acceptance
/// error because stale secrets or workers invalidate subsequent runs.
fn remove_tree(root: &Path) -> Result<(), String> {
    fs::remove_dir_all(root).map_err(|error| format!("fixture cleanup failed: {error}"))?;
    if root.exists() {
        return Err("fixture root remained after cleanup".into());
    }
    Ok(())
}

#[cfg(test)]
mod scope_lifecycle_tests {
    use super::*;

    fn fixture_scope() -> (PathBuf, ProbeScope, ScopeIdentity) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-scope-lifecycle-{nonce}"));
        fs::create_dir(&root).expect("test scope directory");
        let path = root.join("ja-sandbox-probe-scope-test.scope");
        let entry = ScopeEntry {
            pid: 4242,
            pgid: 4242,
            start_identity: "Mon Jan 1 00:00:00 2026".into(),
            comms: vec!["ja-sandbox-worker".into()],
        };
        write_scope_file(
            &path,
            root.to_str().expect("test root"),
            std::slice::from_ref(&entry),
        )
        .expect("write test scope");
        let metadata = fs::symlink_metadata(&path).expect("stat test scope");
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        sync_scope_directory(&path).expect("sync test scope");
        let identity = ScopeIdentity {
            pid: entry.pid,
            pgid: entry.pgid,
            start_identity: entry.start_identity.clone(),
        };
        (
            root.clone(),
            ProbeScope {
                path,
                root: root.to_str().expect("test root").into(),
                entries: vec![entry],
                active: true,
                file_identity: Some(ScopeFileIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                    nlink: metadata.nlink(),
                    mode: metadata.permissions().mode() & 0o777,
                    uid: metadata.uid(),
                }),
            },
            identity,
        )
    }

    /// A completed final child removes only the now-empty active scope and
    /// leaves the directory entry durable for the next lifecycle registration.
    #[test]
    fn final_unregister_removes_active_scope() {
        let (root, mut scope, identity) = fixture_scope();
        scope.unregister_child(&identity).expect("unregister child");
        assert!(!scope.active);
        assert!(scope.entries.is_empty());
        assert!(!scope.path.exists());
        fs::remove_dir(root).expect("remove test scope directory");
    }

    /// Identity mismatch cannot remove or rewrite evidence, preventing PID
    /// reuse from being silently accepted by an unregister caller.
    #[test]
    fn unregister_mismatch_retains_evidence() {
        let (root, mut scope, mut identity) = fixture_scope();
        identity.start_identity = "Tue Jan 2 00:00:00 2026".into();
        assert!(scope.unregister_child(&identity).is_err());
        assert!(scope.active);
        assert_eq!(scope.entries.len(), 1);
        assert!(scope.path.exists());
        fs::remove_dir_all(root).expect("remove test scope directory");
    }

    /// A scope can be reactivated after its previous child unregisters; the
    /// next registration never depends on a stale global scope inode.
    #[test]
    fn scope_reactivates_after_final_unregister() {
        let (root, mut scope, identity) = fixture_scope();
        scope.unregister_child(&identity).expect("unregister child");
        scope.ensure_active_scope().expect("reactivate scope");
        assert!(scope.active);
        assert!(scope.path.exists());
        fs::remove_dir_all(root).expect("remove test scope directory");
    }

    /// A mode fault rejects a candidate commit and keeps the in-memory child
    /// identity, so a register/unregister caller cannot report false cleanup.
    #[test]
    fn commit_mode_fault_is_fail_closed() {
        let (root, mut scope, _identity) = fixture_scope();
        fs::set_permissions(&scope.path, fs::Permissions::from_mode(0o644))
            .expect("make test fault");
        let candidate = ScopeEntry {
            pid: 4343,
            pgid: 4343,
            start_identity: "Mon Jan 1 00:00:00 2026".into(),
            comms: vec!["ja-sandbox-worker".into()],
        };
        assert!(scope.commit_with_restore(&[candidate]).is_err());
        assert!(scope.entries.len() == 1);
        fs::remove_dir_all(root).expect("remove test scope directory");
    }

    /// Report parsing must reject reserved PID values before liveness or
    /// direct cleanup checks can be attempted.
    #[test]
    fn report_pid_reserved_values_fail_closed() {
        assert_eq!(parse_report_pid("idle=true\npid=42", "missing"), Ok(42));
        assert!(parse_report_pid("pid=42", "missing").is_err());
        for value in ["-1", "0", "1"] {
            assert!(parse_report_pid(&format!("pid={value}"), "missing").is_err());
        }
    }

    /// The setsid parser accepts only the two complete worker records; every
    /// unknown, duplicate, empty, trailing, or mutually exclusive field is a
    /// cleanup failure rather than evidence that no descendant exists.
    #[test]
    fn setsid_report_grammar_is_exact() {
        assert_eq!(
            parse_setsid_report("setsid-denied=true"),
            Ok(SetsidReport::Denied)
        );
        assert_eq!(
            parse_setsid_report("setsid-started=true\npid=42"),
            Ok(SetsidReport::Descendant { pid: 42 })
        );
        for invalid in [
            "setsid-started=true\npid=42\nextra=true",
            "setsid-denied=true\nextra=true",
            "setsid-started=true\nsetsid-started=true\npid=42",
            "setsid-started=true\npid=42\npid=43",
            "setsid-denied=true\npid=42",
            "setsid-denied=true\nsetsid-started=true",
            "setsid-started=false\npid=42",
            "setsid-denied=false",
            "setsid-started=true\npid=1",
            "setsid-started=true\npid=+42",
            "setsid-started=true\npid=42\n",
            "setsid-started=true\n\npid=42",
            "setsid-started=true\npid=42\nsetsid-denied=true",
        ] {
            assert!(
                parse_setsid_report(invalid).is_err(),
                "accepted: {invalid:?}"
            );
        }
    }

    /// A setsid report is accepted only when the queried PID became its own
    /// new process-group leader; matching the wrapper PGID would otherwise
    /// let the denied pre_exec path masquerade as an escaped-session test.
    #[test]
    fn escaped_identity_requires_new_process_group() {
        let evidence = EscapeEvidence {
            path: PathBuf::new(),
            identity: ScopeFileIdentity {
                dev: 1,
                ino: 2,
                nlink: 1,
                mode: 0o600,
                uid: 3,
            },
            contents: Vec::new(),
            operation_id: "setsid".into(),
            parent_pid: 7,
            parent_pgid: 8,
            nonce: 9,
            recovery_report: None,
        };
        let valid = ControlledProcessIdentity {
            pid: 42,
            pgid: 42,
            start_identity: "Mon Jan 1 00:00:00 2026".into(),
            comm: "ja-sandbox-worker".into(),
        };
        assert!(escaped_identity_is_valid(&valid, &evidence));
        assert!(!escaped_identity_is_valid(
            &ControlledProcessIdentity {
                pgid: 8,
                ..valid.clone()
            },
            &evidence
        ));
        assert!(!escaped_identity_is_valid(
            &ControlledProcessIdentity {
                pgid: 43,
                ..valid.clone()
            },
            &evidence
        ));
        assert!(!escaped_identity_is_valid(
            &ControlledProcessIdentity {
                pid: 1,
                pgid: 1,
                ..valid
            },
            &evidence
        ));
    }

    /// Provisional escape evidence is durable before spawn and cannot be
    /// mistaken for a captured descendant; reserved parent identities are
    /// rejected before any file image is published.
    #[test]
    fn escape_evidence_provisional_state_is_explicit() {
        let image = escape_evidence_contents("setsid", 42, 43, 7, "provisional", None, None)
            .expect("provisional image");
        assert!(image.contains("operation_id=setsid\n"));
        assert!(image.contains("parent_pid=42\nparent_pgid=43\n"));
        assert!(image.contains("descendant_pid=unknown\n"));
        assert!(image.contains("descendant_pgid=unknown\n"));
        assert!(image.contains("state=provisional\n"));
        assert!(escape_evidence_contents("setsid", 1, 43, 7, "provisional", None, None).is_err());
        assert!(escape_evidence_contents("setsid", 42, 1, 7, "provisional", None, None).is_err());
    }

    /// Captured escape evidence is fixed-field and rejects a reserved or
    /// delimiter-bearing identity before the atomic upgrade can publish it.
    #[test]
    fn escape_evidence_upgrade_identity_is_validated() {
        let identity = ControlledProcessIdentity {
            pid: 44,
            pgid: 45,
            comm: "ja-sandbox-worker".into(),
            start_identity: "Mon Jan 1 00:00:00 2026".into(),
        };
        let image =
            escape_evidence_contents("setsid", 42, 43, 7, "active", Some(&identity), Some("none"))
                .expect("active image");
        assert!(image.contains("descendant_pid=44\n"));
        assert!(image.contains("descendant_pgid=45\n"));
        assert!(
            escape_evidence_contents(
                "setsid",
                42,
                43,
                7,
                "active",
                Some(&ControlledProcessIdentity {
                    pid: 1,
                    ..identity.clone()
                }),
                Some("none")
            )
            .is_err()
        );
        assert!(
            escape_evidence_contents(
                "setsid",
                42,
                43,
                7,
                "failure",
                Some(&ControlledProcessIdentity {
                    comm: "worker\nstate=active".into(),
                    ..identity
                }),
                Some("identity-lost")
            )
            .is_err()
        );
    }

    /// A recovery report is an assertion that a descendant may exist; empty,
    /// missing, or malformed images therefore cannot be treated as “none”.
    #[test]
    fn setsid_recovery_report_faults_fail_closed() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-escape-recovery-{nonce}"));
        fs::create_dir(&root).expect("recovery root");
        let base = EscapeEvidence {
            path: root.join("escape.evidence"),
            identity: ScopeFileIdentity {
                dev: 1,
                ino: 2,
                nlink: 1,
                mode: 0o600,
                uid: 1,
            },
            contents: b"provisional".to_vec(),
            operation_id: "setsid".into(),
            parent_pid: 42,
            parent_pgid: 43,
            nonce: 7,
            recovery_report: None,
        };
        assert!(recover_escaped_identity(&base).is_err());
        for (index, contents) in [b"".as_slice(), b"state=active\n".as_slice(), b"pid=nope\n"]
            .into_iter()
            .enumerate()
        {
            let report = root.join(format!("report-{index}"));
            fs::write(&report, contents).expect("report");
            let evidence = EscapeEvidence {
                recovery_report: Some(report),
                ..base.clone()
            };
            assert!(recover_escaped_identity(&evidence).is_err());
        }
        let missing = EscapeEvidence {
            recovery_report: Some(root.join("missing-report")),
            ..base.clone()
        };
        assert!(recover_escaped_identity(&missing).is_err());
        fs::remove_dir_all(root).expect("recovery cleanup");
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum EscapeFaultPoint {
        RestoreWrite,
        RestoreFileSync,
        Rename,
        ParentSync,
        Unlink,
    }

    struct EscapeFaultPlan {
        points: Vec<EscapeFaultPoint>,
        real: RealEscapeEvidenceIo,
    }

    impl EscapeFaultPlan {
        fn new(point: EscapeFaultPoint) -> Self {
            Self::with_points([point])
        }

        fn with_points(points: impl IntoIterator<Item = EscapeFaultPoint>) -> Self {
            Self {
                points: points.into_iter().collect(),
                real: RealEscapeEvidenceIo,
            }
        }

        fn fails(&self, point: EscapeFaultPoint) -> bool {
            self.points.contains(&point)
        }
    }

    impl EscapeEvidenceIo for EscapeFaultPlan {
        fn write_complete(
            &mut self,
            path: &Path,
            bytes: &[u8],
        ) -> Result<ScopeFileIdentity, String> {
            let identity = self.real.write_complete(path, bytes)?;
            let is_restore_image = path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.ends_with("-restore"));
            if is_restore_image
                && (self.fails(EscapeFaultPoint::RestoreWrite)
                    || self.fails(EscapeFaultPoint::RestoreFileSync))
            {
                Err("escape evidence injected write fault".into())
            } else {
                Ok(identity)
            }
        }

        fn rename(&mut self, from: &Path, to: &Path) -> Result<(), String> {
            if self.fails(EscapeFaultPoint::Rename) {
                return Err("escape evidence injected rename fault".into());
            }
            self.real.rename(from, to)
        }

        fn validate(&mut self, path: &Path, expected: ScopeFileIdentity) -> Result<(), String> {
            self.real.validate(path, expected)
        }

        fn sync_parent(&mut self, path: &Path) -> Result<(), String> {
            if self.fails(EscapeFaultPoint::ParentSync) {
                return Err("escape evidence injected parent sync fault".into());
            }
            self.real.sync_parent(path)
        }

        fn unlink(&mut self, path: &Path) -> Result<(), String> {
            if self.fails(EscapeFaultPoint::Unlink) {
                return Err("escape evidence injected unlink fault".into());
            }
            self.real.unlink(path)
        }
    }

    fn test_escape_evidence(root: &Path, name: &str, nonce: u128) -> EscapeEvidence {
        let path = root.join(name);
        let contents = escape_evidence_contents("setsid", 42, 43, nonce, "provisional", None, None)
            .expect("evidence image");
        let identity = write_escape_file(&path, &contents).expect("evidence file");
        EscapeEvidence {
            path,
            identity,
            contents: contents.into_bytes(),
            operation_id: "setsid".into(),
            parent_pid: 42,
            parent_pgid: 43,
            nonce,
            recovery_report: None,
        }
    }

    const ESCAPE_ABORT_ROOT: &str = "JA_SANDBOX_ESCAPE_ABORT_ROOT";

    /// Enter the real escape finalizer in a separate process so its fixed
    /// abort is observable without killing the test harness; recovery failure
    /// must leave a durable marker and the pending image behind.
    #[test]
    fn escape_post_unlink_restore_failure_is_fixed_abort() {
        if let Some(root) = env::var_os(ESCAPE_ABORT_ROOT) {
            run_escape_abort_child(&PathBuf::from(root));
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-escape-abort-{nonce}"));
        fs::create_dir(&root).expect("abort root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("abort root mode");
        let mut command = Command::new(env::current_exe().expect("test exe"));
        command
            .arg("escape_post_unlink_restore_failure_is_fixed_abort")
            .arg("--nocapture")
            .env(ESCAPE_ABORT_ROOT, &root);
        let output = run_bounded_command(command, Duration::from_secs(5), 16 * 1024, 16 * 1024)
            .expect("bounded escape abort fixture");
        assert!(
            !output.status.success(),
            "fault child unexpectedly succeeded"
        );
        let failure = root.join("ja-sandbox-escape-abort.evidence.recovery-failure");
        assert!(failure.is_file(), "escape failure witness missing");
        let failure_text = fs::read_to_string(&failure).expect("escape failure witness");
        assert!(failure_text.contains("category=evidence-restore\n"));
        assert!(
            root.join(".ja-sandbox-escape-abort.evidence-restore")
                .is_file(),
            "escape restore evidence was not retained"
        );
        assert!(!root.join("ja-sandbox-escape-abort.evidence").exists());
        fs::remove_dir_all(root).expect("abort root cleanup");
    }

    /// Build the real escape evidence and invoke the production remove path;
    /// the combined parent-sync/restore-write fault must reach fixed abort.
    fn run_escape_abort_child(root: &Path) -> ! {
        let evidence = test_escape_evidence(root, "ja-sandbox-escape-abort.evidence", 77);
        let mut io = EscapeFaultPlan::with_points([
            EscapeFaultPoint::ParentSync,
            EscapeFaultPoint::RestoreWrite,
        ]);
        let _ = remove_escape_evidence(&mut io, &evidence);
        panic!("escape finalizer returned after unrecoverable restore");
    }

    /// Run the real escape restore/remove transactions with injected IO
    /// failures; pending or active evidence must remain observable.
    #[test]
    fn escape_evidence_io_faults_retain_evidence() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-escape-io-faults-{nonce}"));
        fs::create_dir(&root).expect("fault root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let restore_faults = [
            EscapeFaultPoint::RestoreWrite,
            EscapeFaultPoint::RestoreFileSync,
            EscapeFaultPoint::Rename,
            EscapeFaultPoint::ParentSync,
        ];
        for (index, point) in restore_faults.into_iter().enumerate() {
            let name = format!("escape-{index}.evidence");
            let evidence = test_escape_evidence(&root, &name, nonce + index as u128 + 1);
            fs::remove_file(&evidence.path).expect("simulate unlink");
            let mut io = EscapeFaultPlan::new(point);
            assert!(restore_escape_evidence(&mut io, &evidence).is_err());
            let pending = evidence.path.with_file_name(format!(".{name}-restore"));
            match point {
                EscapeFaultPoint::ParentSync => assert!(evidence.path.exists()),
                _ => assert!(pending.exists()),
            }
            if pending.exists() {
                fs::remove_file(pending).expect("pending cleanup");
            }
            if evidence.path.exists() {
                fs::remove_file(&evidence.path).expect("active cleanup");
            }
        }
        let evidence = test_escape_evidence(&root, "escape-unlink.evidence", nonce + 10);
        let mut io = EscapeFaultPlan::new(EscapeFaultPoint::Unlink);
        assert!(remove_escape_evidence(&mut io, &evidence).is_err());
        assert!(evidence.path.exists());
        fs::remove_file(evidence.path).expect("unlink fault cleanup");
        fs::remove_dir(root).expect("fault root cleanup");
    }

    /// A post-unlink recovery recreates the held image through a sibling
    /// inode, so a durability fault cannot turn evidence removal into success.
    #[test]
    fn escape_evidence_restore_republishes_held_image() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-escape-restore-{nonce}"));
        fs::create_dir(&root).expect("restore root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let path = root.join("ja-sandbox-escape-restore.evidence");
        let contents = escape_evidence_contents("setsid", 42, 43, 9, "provisional", None, None)
            .expect("image");
        let identity = write_escape_file(&path, &contents).expect("evidence");
        let evidence = EscapeEvidence {
            path: path.clone(),
            identity,
            contents: contents.into_bytes(),
            operation_id: "setsid".into(),
            parent_pid: 42,
            parent_pgid: 43,
            nonce: 9,
            recovery_report: None,
        };
        fs::remove_file(&path).expect("simulate post-unlink state");
        let mut io = RealEscapeEvidenceIo;
        restore_escape_evidence(&mut io, &evidence).expect("restore");
        assert_eq!(fs::read(&path).expect("restored bytes"), evidence.contents);
        let restored = fs::symlink_metadata(&path).expect("restored metadata");
        assert_eq!(restored.permissions().mode() & 0o777, 0o600);
        fs::remove_file(path).expect("cleanup evidence");
        fs::remove_dir(root).expect("cleanup root");
    }

    /// A pre-existing pending inode is never deleted by a failed state
    /// upgrade; the outer residual gate must retain it for diagnosis.
    #[test]
    fn escape_upgrade_retains_pending_on_collision() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("ja-escape-pending-{nonce}"));
        fs::create_dir(&root).expect("pending root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("root mode");
        let path = root.join("ja-sandbox-escape-pending.evidence");
        let provisional = escape_evidence_contents("setsid", 42, 43, 11, "provisional", None, None)
            .expect("provisional");
        let identity = write_escape_file(&path, &provisional).expect("active evidence");
        let pending = path.with_file_name(".ja-sandbox-escape-pending.evidence-pending");
        write_escape_file(&pending, "sentinel\n").expect("pending sentinel");
        let evidence = EscapeEvidence {
            path: path.clone(),
            identity,
            contents: provisional.into_bytes(),
            operation_id: "setsid".into(),
            parent_pid: 42,
            parent_pgid: 43,
            nonce: 11,
            recovery_report: None,
        };
        let mut collision_evidence = evidence.clone();
        assert!(
            upgrade_escape_evidence(&mut collision_evidence, None, "failure", "collision").is_err()
        );
        assert!(pending.exists());
        fs::remove_file(pending).expect("pending cleanup");
        fs::remove_file(path).expect("active cleanup");
        fs::remove_dir(root).expect("root cleanup");
    }
}
