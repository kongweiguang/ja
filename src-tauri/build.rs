// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

/// Lets Tauri generate platform context during the build so the runtime entry
/// remains small and does not duplicate generated capability/configuration code.
fn main() {
    tauri_build::build()
}
