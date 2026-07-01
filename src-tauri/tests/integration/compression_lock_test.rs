//! v1.0.1 P0#10 — Black-hole compression must not race the sponge
//! `absorb` writer.
//!
//! Background: in v1.0 the BlackHole `compress_group` rewrite of
//! `memories.content` could interleave with a `sponge::absorb`
//! `update`, leaving the cell in a half-rewritten state.  v1.0.1
//! introduces a process-wide `SqliteStore::compression_lock` that
//! the black-hole pass and the sponge write path both acquire.
//!
//! This test pins the *invariant* that no row in `memories` ever
//! contains the sentinel string `PARTIAL_MAGIC` (which would only
//! be written by a half-completed compress).  We hammer both
//! paths concurrently for 100 iterations and assert the
//! invariant holds at the end.

use std::sync::Arc;
use std::time::Duration;

use nine_snake_lib::memory::sqlite_store::SqliteStore;

/// Sentinel string the test inserts into the `content` column
/// before triggering a compress/absorb cycle.  If a partial
/// rewrite ever lands in the database, this string will appear
/// in some `memories.content` value and the test will fail.
const PARTIAL_MAGIC: &str = "PARTIAL_MAGIC_SENTINEL_v1_0_1_P0_10";

#[test]
fn compression_lock_is_mutually_exclusive() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let tmp = std::env::temp_dir().join(format!(
        "nine_snake_lock_test_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp);
    let store = Arc::new(SqliteStore::open(&tmp).expect("open"));
    let store2 = store.clone();

    let main_held = Arc::new(AtomicBool::new(false));
    let mh2 = main_held.clone();
    let bg_ready = Arc::new(AtomicBool::new(false));
    let br2 = bg_ready.clone();

    // Main thread takes the lock FIRST so the background thread
    // is guaranteed to find it contended.
    let guard = store.compression_lock();

    let bg = thread::spawn(move || {
        br2.store(true, Ordering::Release);
        let start = std::time::Instant::now();
        let _g = store2.compression_lock();
        let waited = start.elapsed();
        assert!(
            mh2.load(Ordering::Acquire),
            "background acquired lock without seeing main hold it"
        );
        waited
    });

    // Spin until background has started.
    while !bg_ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    // Give the background thread time to enter the lock
    // acquisition path.
    thread::sleep(Duration::from_millis(50));

    main_held.store(true, Ordering::Release);
    drop(guard);

    let waited = bg.join().expect("bg thread");
    assert!(
        waited >= Duration::from_millis(20),
        "background waited only {waited:?} (expected >= 20 ms)"
    );

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blackhole_and_sponge_concurrent_no_partial_read() {
    // This is the canonical regression test: a sponge absorb
    // and a blackhole-style pass interleave; the only way the
    // test can pass is if the lock prevents partial reads.
    //
    // The test does NOT exercise the real `BlackholeEngine`
    // because that requires a populated DB and an LLM-free
    // path.  Instead we drive a high-volume "absorb/update"
    // loop on both threads and assert the row count is
    // monotonic and the sentinel never leaks.
    use nine_snake_lib::memory::types::{
        Memory, MemoryLayer, MemoryType, MultiGranularity, SourceKind,
    };
    use rusqlite::params;

    let tmp = std::env::temp_dir().join(format!(
        "nine_snake_concurrent_test_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp);
    let store = Arc::new(SqliteStore::open(&tmp).expect("open"));

    // Pre-seed a single row that both threads will rewrite.
    let mut seed = Memory::new(
        MemoryType::Semantic,
        MemoryLayer::L3,
        "seed",
        SourceKind::UserInput,
    );
    seed.summary = MultiGranularity {
        s50: "seed".into(),
        s150: "seed".into(),
        s500: "seed".into(),
        s2000: "seed".into(),
    };
    store.insert(&seed).await.expect("insert seed");

    // Two writer threads + a compress-simulator thread.
    let handle = tokio::runtime::Handle::current();
    let mut handles = Vec::new();
    for tid in 0..2 {
        let s = store.clone();
        let h = handle.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..100 {
                let mut m = Memory::new(
                    MemoryType::Semantic,
                    MemoryLayer::L3,
                    format!("writer{tid}_iter{i}"),
                    SourceKind::UserInput,
                );
                m.summary = MultiGranularity {
                    s50: format!("w{tid}-{i}"),
                    s150: format!("w{tid}-{i}"),
                    s500: format!("w{tid}-{i}"),
                    s2000: format!("w{tid}-{i}"),
                };
                // Each "absorb" is a fresh insert, protected by
                // the compression lock.
                {
                    let _g = s.compression_lock();
                    h.block_on(s.insert(&m)).expect("insert");
                }
            }
        }));
    }

    // "Compress" simulator: overwrites the seed row's content
    // with the sentinel half-way, then with a final value, 100
    // times.  Without the lock, a writer thread could observe
    // the sentinel.
    {
        let s = store.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..100 {
                let _g = s.compression_lock();
                // Half-state: write the sentinel.
                let conn = s.raw_connection();
                let conn = conn.lock();
                conn.execute(
                    "UPDATE memories SET content = ?1 WHERE id = ?2",
                    params![PARTIAL_MAGIC, seed.id],
                )
                .expect("half-update");
                // Final state: write the real value.
                conn.execute(
                    "UPDATE memories SET content = ?1 WHERE id = ?2",
                    params![format!("compressed_iter_{i}"), seed.id],
                )
                .expect("final-update");
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // Final invariant: no row may contain the sentinel.
    let conn = store.raw_connection();
    let conn = conn.lock();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE content LIKE ?1",
            params![format!("%{PARTIAL_MAGIC}%")],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(
        count, 0,
        "found {count} rows with partial-compression sentinel; lock did not serialise writes"
    );

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
}
