//! Integration tests for the v0.5 Work engine.
//!
//! Covers: create / move / delete / time tracking / priority
//! recommendation.  We don't exercise the L3 mirror (the engine is
//! created without a SpongeEngine).

use super::common::TmpStore;
use nine_snake_lib::work::{recommend_priority, summarise_meeting, TaskStatus, WorkEngine};

fn engine(store: &TmpStore) -> WorkEngine {
    WorkEngine::new(store.store.clone())
}

#[test]
fn create_and_list_round_trip() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let t = eng
        .create_task("Buy milk".into(), "from corner store".into(), Some(1), None)
        .expect("create");
    assert_eq!(t.title, "Buy milk");
    assert_eq!(t.status, TaskStatus::Todo);
    let list = eng.list_tasks(None, Some(10)).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, t.id);
}

#[test]
fn status_transition_records_completion() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let t = eng
        .create_task("T".into(), "".into(), None, None)
        .expect("create");
    let doing = eng.set_status(&t.id, TaskStatus::Doing).expect("doing");
    assert_eq!(doing.status, TaskStatus::Doing);
    let done = eng.set_status(&t.id, TaskStatus::Done).expect("done");
    assert_eq!(done.status, TaskStatus::Done);
    assert!(done.completed_at.is_some());
}

#[test]
fn timer_lifecycle_and_accumulation() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let t = eng
        .create_task("T".into(), "".into(), None, None)
        .expect("create");
    eng.start_timer(&t.id).expect("start");
    assert_eq!(eng.active_timer(), Some(t.id.clone()));
    eng.add_time(&t.id, 1500).expect("add");
    eng.stop_timer().expect("stop");
    assert_eq!(eng.active_timer(), None);
    let reload = eng.get_task(&t.id).expect("get").expect("exists");
    assert_eq!(reload.time_spent_ms, 1500);
}

#[test]
fn priority_recommendation_handles_urgency() {
    let now = chrono::Utc::now().timestamp();
    let due_soon = now + 3600;
    let p1 = recommend_priority("紧急 fix", Some(due_soon));
    let p2 = recommend_priority("write weekly report", None);
    assert!(p1 > p2, "urgency should yield higher priority");
    assert!(p1 <= 3);
    assert!(p2 >= 0);
}

#[test]
fn meeting_summary_extracts_actions() {
    let t = "- alice: design\n- bob: review\nagreed on Rust\nnext week: release\n";
    let mm = summarise_meeting(t);
    assert_eq!(mm.actions.len(), 2);
    assert!(mm.decisions.iter().any(|d| d.contains("Rust")));
}

#[test]
fn update_task_clears_due_date() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let due = chrono::Utc::now().timestamp() + 86400;
    let t = eng
        .create_task("T".into(), "".into(), None, Some(due))
        .expect("create");
    assert_eq!(t.due_at, Some(due));
    let updated = eng
        .update_task(&t.id, None, None, None, Some(None))
        .expect("update");
    assert_eq!(updated.due_at, None);
}

#[test]
fn delete_task_removes_row() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let t = eng
        .create_task("T".into(), "".into(), None, None)
        .expect("create");
    assert!(eng.delete_task(&t.id).expect("delete"));
    assert!(eng.get_task(&t.id).expect("get").is_none());
}
