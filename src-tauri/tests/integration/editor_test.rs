//! Integration tests for the v0.5 editor surface area.
//!
//! Covers: `editor_open` / `editor_read` / `editor_write` /
//! `editor_list` + the workspace-root path sandboxing.  Git
//! integration is covered in a separate file because it shells out
//! to the system `git` binary and may be skipped in CI.

use nine_snake_lib::editor::EditorState;
use std::fs;

fn fresh() -> (tempfile::TempDir, EditorState) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let state = EditorState::new(dir.path()).expect("new editor state");
    (dir, state)
}

#[test]
fn open_read_write_round_trip() {
    let (_dir, state) = fresh();
    let written = state.write_file("hello.txt", "hello world").expect("write");
    assert_eq!(written.content, "hello world");
    let read = state.read_file("hello.txt").expect("read");
    assert_eq!(read.content, "hello world");
    assert_eq!(read.size, "hello world".len() as u64);
}

#[test]
fn write_creates_parent_directories() {
    let (_dir, state) = fresh();
    state
        .write_file("deep/nested/dir/file.rs", "fn main() {}")
        .expect("write deep");
    let read = state.read_file("deep/nested/dir/file.rs").expect("read");
    assert_eq!(read.content, "fn main() {}");
}

#[test]
fn read_rejects_path_outside_workspace() {
    let (dir, state) = fresh();
    // Create a file *outside* the workspace.
    let outside = dir.path().parent().unwrap().join("nine_snake_outside.txt");
    fs::write(&outside, "should not be readable").expect("write outside");
    let result = state.read_file(outside.to_str().unwrap());
    assert!(result.is_err(), "expected error for path outside workspace");
}

#[test]
fn list_tree_returns_files_and_directories() {
    let (_dir, state) = fresh();
    fs::create_dir(state.workspace_root().join("src")).unwrap();
    fs::write(
        state.workspace_root().join("src").join("main.rs"),
        "fn main(){}",
    )
    .unwrap();
    fs::write(state.workspace_root().join("README.md"), "# test").unwrap();
    let tree = state.list_tree(Some(3)).expect("list");
    let paths: Vec<&str> = tree.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"src"));
    assert!(paths.contains(&"src/main.rs"));
    assert!(paths.contains(&"README.md"));
    // src should be marked as a directory.
    let src = tree.iter().find(|e| e.path == "src").unwrap();
    assert!(src.is_dir);
}

#[test]
fn list_tree_skips_node_modules() {
    let (_dir, state) = fresh();
    fs::create_dir(state.workspace_root().join("node_modules")).unwrap();
    fs::write(
        state.workspace_root().join("node_modules").join("pkg.js"),
        "module.exports = 1;",
    )
    .unwrap();
    fs::write(state.workspace_root().join("index.js"), "// real").unwrap();
    let tree = state.list_tree(Some(3)).expect("list");
    let paths: Vec<&str> = tree.iter().map(|e| e.path.as_str()).collect();
    assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
    assert!(paths.contains(&"index.js"));
}

#[test]
fn file_size_is_returned() {
    let (_dir, state) = fresh();
    let content = "a".repeat(1024);
    state.write_file("big.txt", &content).unwrap();
    let read = state.read_file("big.txt").expect("read");
    assert_eq!(read.size, 1024);
}
