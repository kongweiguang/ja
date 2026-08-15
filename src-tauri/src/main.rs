// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

// Keep release builds attached to the desktop window instead of opening a
// second console, while debug builds retain a console for local diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Delegates composition to the library so Tauri has one native entrypoint
/// across desktop targets and future mobile-specific launch attributes.
fn main() {
    ja_lib::run()
}
