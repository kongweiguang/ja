// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Native Seatbelt orchestration imports setup and probe cases by responsibility.

use ja_macos_sandbox_spike::{
    RunOutput, SandboxChild, SandboxError, SandboxSpec, marker_cleanup::ControlledProcessIdentity,
    marker_cleanup::query_controlled_identity, marker_cleanup::terminate_controlled_identity,
    process_group_is_gone, process_is_alive, run_bounded_command, spawn,
};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
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

unsafe extern "C" {
    fn geteuid() -> u32;
    fn getpgrp() -> i32;
}
