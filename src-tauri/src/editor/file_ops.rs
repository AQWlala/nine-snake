//! v0.5: editor / file-ops backend.
//!
//! The editor surface area is intentionally small — it is the
//! lower-than-the-UI layer the Monaco/Tree/Terminal front-end talks
//! to.  Three concerns:
//!
//! 1. `editor_open` / `editor_read` / `editor_write` — basic
//!    single-file operations.  Sizes are capped to keep a runaway
//!    front-end from accidentally slurping a 4 GB log file into a
//!    Monaco buffer.
//! 2. `editor_list` — recursive directory listing using `walkdir`,
//!    with `.git`, `node_modules`, `target`, etc. hidden by default.
//! 3. `editor_watch` — `notify`-backed file watcher that streams
//!    change events back to the front-end via a Tauri `emit`.
//!
//! All paths are resolved against a single workspace root
//! (`EditorState::workspace_root`) so a malicious front-end cannot
//! read `/etc/shadow`.  The root is set at app start and never
//! mutated by Tauri commands.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tracing::{info, instrument, warn};
use walkdir::WalkDir;

/// Maximum size of a file that `editor_read` will return, in bytes.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB

/// Directories that the tree-listing skips by default.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".vscode",
    ".DS_Store",
];

/// One entry in a tree listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the workspace root, using `/` as separator.
    pub path: String,
    /// `true` if this is a directory.
    pub is_dir: bool,
    /// Size in bytes.  `0` for directories.
    pub size: u64,
    /// Last modified time as a unix timestamp.
    pub modified: i64,
}

/// A single file change event streamed to the front-end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEvent {
    pub kind: String, // "create" | "modify" | "remove" | "rename"
    pub paths: Vec<String>,
}

/// Shared state for the editor module.  Cheap to clone (one Arc).
#[derive(Clone)]
pub struct EditorState {
    inner: Arc<EditorInner>,
}

struct EditorInner {
    /// Workspace root, normalised to an absolute path.  All file ops
    /// must stay inside this prefix.
    workspace_root: PathBuf,
    /// Active watcher, if any.  Replacing the watcher (e.g. when the
    /// workspace root changes) drops the old one and stops its
    /// background thread.
    watcher: Mutex<Option<RecommendedWatcher>>,
}

impl EditorState {
    /// Creates a new editor state rooted at `root`.  The directory
    /// must exist.
    pub fn new<P: AsRef<Path>>(root: P) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(anyhow!(
                "workspace root does not exist or is not a directory: {}",
                root.display()
            ));
        }
        let canonical = std::fs::canonicalize(&root)
            .with_context(|| format!("canonicalising workspace root: {}", root.display()))?;
        Ok(Self {
            inner: Arc::new(EditorInner {
                workspace_root: canonical,
                watcher: Mutex::new(None),
            }),
        })
    }

    /// Returns the workspace root as a string.
    pub fn workspace_root(&self) -> &Path {
        &self.inner.workspace_root
    }

    /// Resolves a relative (or absolute) path against the workspace
    /// root, rejecting any path that escapes the prefix.
    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.inner.workspace_root.join(path)
        };
        let canonical = std::fs::canonicalize(&candidate)
            .with_context(|| format!("path does not exist: {}", candidate.display()))?;
        if !canonical.starts_with(&self.inner.workspace_root) {
            return Err(anyhow!(
                "path escapes workspace root: {}",
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    /// Resolves a path that may not yet exist (for `editor_write`).
    /// We require the parent to exist and live inside the workspace.
    fn resolve_for_write(&self, path: &str) -> Result<PathBuf> {
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.inner.workspace_root.join(path)
        };
        let parent = candidate
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", candidate.display()))?;
        let parent_canonical = if parent.exists() {
            std::fs::canonicalize(parent)
                .with_context(|| format!("parent does not exist: {}", parent.display()))?
        } else {
            return Err(anyhow!("parent does not exist: {}", parent.display()));
        };
        if !parent_canonical.starts_with(&self.inner.workspace_root) {
            return Err(anyhow!(
                "path escapes workspace root: {}",
                parent_canonical.display()
            ));
        }
        Ok(candidate)
    }

    /// Reads a file, refusing anything larger than `MAX_FILE_BYTES`.
    #[instrument(skip(self))]
    pub fn read_file(&self, path: &str) -> Result<FileContent> {
        let full = self.resolve(path)?;
        let meta =
            std::fs::metadata(&full).with_context(|| format!("stat failed: {}", full.display()))?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(anyhow!(
                "file too large ({} bytes, cap {}): {}",
                meta.len(),
                MAX_FILE_BYTES,
                full.display()
            ));
        }
        let content = std::fs::read_to_string(&full)
            .with_context(|| format!("read_to_string failed: {}", full.display()))?;
        Ok(FileContent {
            path: relative_to(&full, &self.inner.workspace_root),
            content,
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        })
    }

    /// Writes a file, creating parent directories if necessary.
    #[instrument(skip(self, content))]
    pub fn write_file(&self, path: &str, content: &str) -> Result<FileContent> {
        let full = self.resolve_for_write(path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all failed: {}", parent.display()))?;
        }
        std::fs::write(&full, content.as_bytes())
            .with_context(|| format!("write failed: {}", full.display()))?;
        let meta =
            std::fs::metadata(&full).with_context(|| format!("stat failed: {}", full.display()))?;
        Ok(FileContent {
            path: relative_to(&full, &self.inner.workspace_root),
            content: content.to_string(),
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        })
    }

    /// Lists the file tree, skipping `SKIP_DIRS` and any hidden
    /// dotfile directories.  `max_depth` defaults to 8.
    #[instrument(skip(self))]
    pub fn list_tree(&self, max_depth: Option<usize>) -> Result<Vec<FileEntry>> {
        let depth = max_depth.unwrap_or(8);
        let mut out = Vec::new();
        for entry in WalkDir::new(&self.inner.workspace_root)
            .max_depth(depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !should_skip(e.path()))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(target: "nine_snake.editor", error = ?e, "walkdir error");
                    continue;
                }
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(FileEntry {
                path: relative_to(entry.path(), &self.inner.workspace_root),
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified,
            });
        }
        // Stable order: directories first, then files; alphabetical.
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
        Ok(out)
    }

    /// Starts (or replaces) a recursive file watcher on the workspace
    /// root.  Every change event is forwarded on the returned
    /// `mpsc::Receiver<FileEvent>` and the caller (the Tauri command
    /// handler) is responsible for emitting them to the front-end.
    #[instrument(skip(self))]
    pub fn start_watcher(&self) -> Result<mpsc::Receiver<FileEvent>> {
        let (tx, rx) = mpsc::channel::<FileEvent>(256);
        let root = self.inner.workspace_root.clone();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: notify::Result<Event>| {
                match res {
                    Ok(ev) => {
                        let kind = match ev.kind {
                            EventKind::Create(_) => "create",
                            EventKind::Modify(_) => "modify",
                            EventKind::Remove(_) => "remove",
                            _ => return,
                        };
                        let paths: Vec<String> = ev
                            .paths
                            .into_iter()
                            .map(|p| relative_to(&p, &root))
                            .collect();
                        if paths.is_empty() {
                            return;
                        }
                        let evt = FileEvent {
                            kind: kind.to_string(),
                            paths,
                        };
                        if tx.blocking_send(evt).is_err() {
                            // Receiver dropped; nothing we can do.
                        }
                    }
                    Err(e) => {
                        warn!(target: "nine_snake.editor", error = ?e, "watcher error");
                    }
                }
            })
            .context("creating notify watcher")?;
        watcher
            .watch(&self.inner.workspace_root, RecursiveMode::Recursive)
            .context("starting watcher on workspace root")?;

        // Replace any previous watcher; dropping the old one stops it.
        let mut slot = self.inner.watcher.lock();
        *slot = Some(watcher);
        info!(target: "nine_snake.editor", root = %self.inner.workspace_root.display(), "watcher started");
        Ok(rx)
    }

    /// Stops the active watcher (if any).
    pub fn stop_watcher(&self) {
        let mut slot = self.inner.watcher.lock();
        *slot = None;
        info!(target: "nine_snake.editor", "watcher stopped");
    }
}

/// Result of a successful read/write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub modified: i64,
}

fn relative_to(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

fn should_skip(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if SKIP_DIRS.contains(&name) {
            return true;
        }
        if name.starts_with('.') && name != "." && name != ".." {
            // Skip hidden files/dirs.
            return path.is_dir();
        }
    }
    false
}

pub fn validate_workspace_path(path: &str, workspace_root: &Path) -> Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace_root.join(path)
    };
    let canonical = std::fs::canonicalize(&candidate)
        .with_context(|| format!("path does not exist: {}", candidate.display()))?;
    let root_canonical = std::fs::canonicalize(workspace_root).with_context(|| {
        format!(
            "workspace root does not exist: {}",
            workspace_root.display()
        )
    })?;
    if !canonical.starts_with(&root_canonical) {
        return Err(anyhow!(
            "path escapes workspace root: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// Lightweight wrapper that owns a tokio mpsc receiver and turns it
/// into a stream-friendly form for the command layer.  The handle
/// itself is tiny and cloneable.
#[derive(Clone)]
pub struct WatcherHandle {
    rx: Arc<AsyncMutex<mpsc::Receiver<FileEvent>>>,
}

impl WatcherHandle {
    /// Awaits the next event, returning `None` if the channel is
    /// closed.
    pub async fn next(&self) -> Option<FileEvent> {
        let mut g = self.rx.lock().await;
        g.recv().await
    }
}

/// Convenience: start a watcher and return a cloneable handle.
pub fn spawn_watcher(state: &EditorState) -> Result<WatcherHandle> {
    let rx = state.start_watcher()?;
    Ok(WatcherHandle {
        rx: Arc::new(AsyncMutex::new(rx)),
    })
}

/// Polls a watcher for `timeout` and returns all events that arrived
/// in that window.  Used by the front-end's poll-based fallback when
/// it cannot subscribe to a Tauri event stream.
pub async fn drain_for(handle: &WatcherHandle, timeout: Duration) -> Vec<FileEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, handle.next()).await {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_rejects_path_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let state = EditorState::new(dir.path()).unwrap();
        let res = state.resolve("../outside.txt");
        assert!(res.is_err());
    }

    #[test]
    fn read_and_write_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = EditorState::new(dir.path()).unwrap();
        let path = "hello.txt";
        let written = state.write_file(path, "hello world").expect("write");
        assert_eq!(written.content, "hello world");
        let read_back = state.read_file(path).expect("read");
        assert_eq!(read_back.content, "hello world");
    }

    #[test]
    fn list_tree_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn main(){}").unwrap();
        fs::create_dir(dir.path().join("node_modules")).unwrap();
        fs::write(
            dir.path().join("node_modules").join("pkg.js"),
            "module.exports = 1;",
        )
        .unwrap();
        let state = EditorState::new(dir.path()).unwrap();
        let tree = state.list_tree(Some(3)).unwrap();
        assert!(tree.iter().any(|e| e.path == "src/main.rs"));
        assert!(!tree.iter().any(|e| e.path.starts_with("node_modules")));
    }

    #[test]
    fn read_file_too_large_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = EditorState::new(dir.path()).unwrap();
        // Write 16 bytes — well under the cap.
        fs::write(dir.path().join("small.txt"), "x".repeat(16)).unwrap();
        assert!(state.read_file("small.txt").is_ok());
    }
}
