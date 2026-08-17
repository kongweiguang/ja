// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Read-only workspace primitives used by the desktop workbench.
//!
//! The module deliberately has no Tauri dependency.  Keeping the registry,
//! path policy, bounded readers and change detector here lets the future IPC
//! owner expose opaque workspace ids without making filesystem rules depend on
//! a WebView command handler.

mod changes;
mod content;
mod error;
mod model;
mod registry;
mod search;
mod tree;

pub use changes::{
    ChangeBatch, ChangeDetector, ChangeKind, ChangeRecord, PollState, PollingChangeDetector,
    PollingPolicy,
};
pub use content::{ContentPolicy, FileReader};
pub use error::WorkspaceError;
pub use model::{
    ContentKind, EntryKind, FileContent, FileMetadata, FileRevision, SearchHit, TextEncoding,
    TreeEntry, TreePage,
};
pub use registry::{WorkspaceHandle, WorkspaceId, WorkspaceInfo, WorkspaceRegistry};
pub(crate) use registry::{is_reparse_point, path_is_within, reject_link_components};
pub use search::{SearchPolicy, TextSearch, TextSearchResult};
pub use tree::{TreePageRequest, TreePolicy, TreeReader};

#[cfg(test)]
mod tests;
