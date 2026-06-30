//! v0.5: editor subsystem — file operations, file watching, and
//! a thin Git wrapper.  See the submodules for the surface area:
//!
//! * [`file_ops`] — open/read/write/list/watch with workspace-root
//!   path sandboxing.
//! * [`git`] — `status` / `log` / `diff` / `commit` over the `git`
//!   CLI (no native libgit2 dependency).
//! * [`debounce`] — v1.0: coalesce bursts of `FileEvent`s so the
//!   front-end doesn't re-render the tree on every single
//!   `git checkout` event.

pub mod debounce;
pub mod file_ops;
pub mod git;

pub use debounce::spawn_debounced;
pub use file_ops::{
    drain_for, spawn_watcher, validate_workspace_path, EditorState, FileContent, FileEntry,
    FileEvent, WatcherHandle,
};
pub use git::{
    commit as git_commit, diff as git_diff, init as git_init, log as git_log, status as git_status,
    GitDiff, GitLogEntry, GitStatus, StatusEntry,
};
