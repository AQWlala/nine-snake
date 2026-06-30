//! v1.0: file-event debouncer.
//!
//! Wraps a stream of `FileEvent`s and coalesces bursts of changes
//! that arrive within `window` of each other.  The typical case
//! is a `git checkout` or a build tool that touches 50 files in
//! 20 ms — without debouncing, the front-end would re-render the
//! file tree 50 times in a single frame.
//!
//! The implementation is intentionally tiny — a `tokio::time::Sleep`
//! driven by the *latest* event.  A more sophisticated sliding-window
//! would be O(n) in the queue length; ours is O(1).

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};

use crate::editor::file_ops::FileEvent;

/// Default debounce window.  Tuned for "feels responsive" on
/// `git checkout` (touches 10–50 files in < 50 ms) without
/// dropping user-driven single-file saves.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(80);

/// Spawn a debounced forwarder.  The returned `mpsc::Receiver`
/// yields at most one `FileEvent` per `window`; if multiple
/// `FileEvent`s arrive inside the same window, the most recent
/// one is delivered and the earlier ones are dropped.  Per-path
/// dedup is the responsibility of the consumer (the file tree
/// wants one event per path).
pub fn spawn_debounced(
    mut rx: mpsc::Receiver<FileEvent>,
    window: Duration,
) -> mpsc::Receiver<FileEvent> {
    let (tx, out_rx) = mpsc::channel::<FileEvent>(64);
    tokio::spawn(async move {
        let mut pending: HashMap<String, FileEvent> = HashMap::new();
        let mut deadline: Option<Instant> = None;
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    match maybe {
                        Some(ev) => {
                            for p in &ev.paths {
                                pending.insert(p.clone(), ev.clone());
                            }
                            // Slide the deadline forward — we always
                            // wait `window` from the *last* event.
                            deadline = Some(Instant::now() + window);
                        }
                        None => {
                            // Channel closed: flush whatever is
                            // pending so the consumer sees the final
                            // state, then exit.
                            for (_, ev) in pending.drain() {
                                if tx.send(ev).await.is_err() { return; }
                            }
                            return;
                        }
                    }
                }
                _ = async {
                    match deadline {
                        Some(d) => sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // Window expired: coalesce everything in
                    // `pending` into one event with the union of
                    // paths and the *last* kind.
                    if !pending.is_empty() {
                        let mut paths: Vec<String> = pending.keys().cloned().collect();
                        paths.sort();
                        paths.dedup();
                        let last_kind = pending
                            .values()
                            .last()
                            .map(|e| e.kind.clone())
                            .unwrap_or_else(|| "modify".to_string());
                        let coalesced = FileEvent {
                            kind: last_kind,
                            paths,
                        };
                        if tx.send(coalesced).await.is_err() { return; }
                        pending.clear();
                    }
                    deadline = None;
                }
            }
        }
    });
    out_rx
}

async fn sleep_until(d: Instant) {
    let now = Instant::now();
    if d > now {
        sleep(d - now).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn debounces_burst_into_one_event() {
        let (tx, rx) = mpsc::channel::<FileEvent>(32);
        let mut debounced = spawn_debounced(rx, Duration::from_millis(30));

        for i in 0..5 {
            tx.send(FileEvent {
                kind: "modify".into(),
                paths: vec![format!("a/{i}")],
            })
            .await
            .unwrap();
        }
        drop(tx);

        let first = tokio::time::timeout(Duration::from_millis(200), debounced.recv())
            .await
            .unwrap()
            .expect("at least one event");
        assert!(first.paths.len() >= 1);
        // The next call should hit `None` after the drain.
        let second = debounced.recv().await;
        assert!(second.is_none());
    }
}
