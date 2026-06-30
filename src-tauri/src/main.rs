//! nine-snake Tauri 2.0 application entry point.
//!
//! Responsibilities of this binary are intentionally minimal: it just
//! initialises the async runtime, installs the `tracing` subscriber, and
//! hands off to [`nine_snake_lib::run`]. All actual logic lives in
//! `src/lib.rs` and its submodules so the same code paths can be reused by
//! integration tests and (in the future) by an alternative front-end.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    nine_snake_lib::run();
}
