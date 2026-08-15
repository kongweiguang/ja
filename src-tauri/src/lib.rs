// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Keep a single composition root so plugins, managed state, and shutdown
// resources can later be registered exactly once and reviewed as a unit.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
