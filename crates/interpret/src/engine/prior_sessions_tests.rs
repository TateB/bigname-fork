use std::collections::HashMap;

use super::{SessionKey, retain, take_resumable};
use crate::RunMode;

fn key(chain_id: &str, from_block: i64, mode: RunMode) -> SessionKey {
    SessionKey {
        chain_id: chain_id.to_owned(),
        from_block,
        mode,
    }
}

#[test]
fn a_session_resumes_only_the_next_batch_of_its_own_request() {
    let normal = key("chain", 0, RunMode::Normal);
    for (resumed, next_block, allow_resume) in [
        (normal.clone(), 11, true),
        (key("chain", 5, RunMode::Normal), 11, true),
        (key("chain", 0, RunMode::Redo), 11, true),
        (normal.clone(), 12, true),
        (normal.clone(), 11, false),
    ] {
        let mut sessions = HashMap::new();
        retain(&mut sessions, normal.clone(), 11, "normal", false);
        let expected = (resumed == normal && next_block == 11 && allow_resume).then_some("normal");
        assert_eq!(
            take_resumable(&mut sessions, &resumed, next_block, allow_resume),
            expected,
            "{resumed:?} at {next_block}, resume allowed {allow_resume}"
        );
        assert!(sessions.is_empty(), "a take always empties the chain slot");
    }
}

#[test]
fn a_completed_redo_hands_its_session_to_the_next_normal_batch() {
    let redo = key("chain", 9, RunMode::Redo);
    for (resumed, next_block, allow_resume, expected) in [
        (key("chain", 0, RunMode::Normal), 11, true, Some("redo")),
        (key("chain", 0, RunMode::Normal), 12, true, None),
        (key("chain", 0, RunMode::Normal), 11, false, None),
        (redo.clone(), 11, true, None),
        (key("chain", 0, RunMode::RecomputeFlags), 11, true, None),
    ] {
        let mut sessions = HashMap::new();
        retain(&mut sessions, redo.clone(), 11, "redo", true);
        assert_eq!(
            take_resumable(&mut sessions, &resumed, next_block, allow_resume),
            expected,
            "{resumed:?} at {next_block}, resume allowed {allow_resume}"
        );
    }
}

#[test]
fn an_unfinished_redo_resumes_only_its_own_next_batch() {
    let redo = key("chain", 9, RunMode::Redo);
    let mut sessions = HashMap::new();
    retain(&mut sessions, redo.clone(), 11, "redo", false);
    assert_eq!(
        take_resumable(&mut sessions, &key("chain", 0, RunMode::Normal), 11, true),
        None
    );
    retain(&mut sessions, redo.clone(), 11, "redo", false);
    assert_eq!(take_resumable(&mut sessions, &redo, 11, true), Some("redo"));
}

#[test]
fn each_chain_keeps_one_session() {
    let mut sessions = HashMap::new();
    retain(
        &mut sessions,
        key("chain", 0, RunMode::Normal),
        11,
        "first",
        false,
    );
    retain(
        &mut sessions,
        key("other", 0, RunMode::Normal),
        11,
        "other",
        false,
    );
    retain(
        &mut sessions,
        key("chain", 0, RunMode::Normal),
        21,
        "second",
        false,
    );

    assert_eq!(sessions.len(), 2);
    assert_eq!(
        take_resumable(&mut sessions, &key("chain", 0, RunMode::Normal), 21, true),
        Some("second")
    );
    assert_eq!(
        take_resumable(&mut sessions, &key("other", 0, RunMode::Normal), 11, true),
        Some("other")
    );
}
