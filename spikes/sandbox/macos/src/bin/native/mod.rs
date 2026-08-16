// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Native Seatbelt orchestration imports setup and probe cases by responsibility.

use ja_macos_sandbox_spike::{
    SandboxChild, SandboxError, SandboxSpec, kill_process, process_is_alive, spawn,
};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod diagnostics {
    include!("../probe_diagnostics/mod.rs");
}
use diagnostics::SandboxDenialDiagnostics;

const MARKER: &str = "JA_PARENT_SECRET_MARKER";

macro_rules! argv {
    ($($value:expr),* $(,)?) => {
        vec![$($value.into()),*]
    };
}

include!("setup.rs");
include!("probes.rs");
