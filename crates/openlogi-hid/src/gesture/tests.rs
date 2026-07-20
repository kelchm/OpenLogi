#![allow(clippy::expect_used, reason = "expect/unwrap are idiomatic in tests")]

use super::*;

fn press() -> RawControlEvent {
    RawControlEvent::DivertedButtons([reprog_controls::GESTURE_BUTTON_CID, 0, 0, 0])
}

fn release() -> RawControlEvent {
    RawControlEvent::DivertedButtons([0, 0, 0, 0])
}

#[test]
fn quick_tap_is_a_click_even_while_the_cursor_moves() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();

    handle_reprog(&mut acc, press(), &[], false, &tx);
    handle_reprog(
        &mut acc,
        RawControlEvent::RawXy { dx: 120, dy: 5 },
        &[],
        false,
        &tx,
    );
    handle_reprog(&mut acc, release(), &[], false, &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture {
            button: ButtonId::GestureButton,
            direction: GestureDirection::Click,
        })
    );
    assert!(
        rx.try_recv().is_err(),
        "a quick tap emits exactly one click"
    );
}

#[test]
fn a_held_gesture_commits_a_swipe_and_does_not_also_click() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();

    handle_reprog(&mut acc, press(), &[], false, &tx);
    // Pretend the button has been held well past the swipe gate.
    acc.gesture_swipe.backdate_hold_for_test();
    handle_reprog(
        &mut acc,
        RawControlEvent::RawXy { dx: 120, dy: 5 },
        &[],
        false,
        &tx,
    );

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture {
            button: ButtonId::GestureButton,
            direction: GestureDirection::Right,
        })
    );

    handle_reprog(&mut acc, release(), &[], false, &tx);
    assert!(
        rx.try_recv().is_err(),
        "a committed swipe must not also click on release"
    );
}

#[test]
fn a_held_dpi_button_presses_once_on_the_rising_edge() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);

    handle_reprog(&mut acc, down, &[dpi], false, &tx);
    handle_reprog(&mut acc, down, &[dpi], false, &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle))
    );
    assert!(rx.try_recv().is_err(), "a held DPI button presses once");
}

#[test]
fn a_dpi_button_re_presses_after_a_release() {
    // Rising-edge detection must re-arm: press → release → press is two
    // distinct presses. The release (a frame without the CID) is what resets
    // the edge; without it a re-press would be swallowed as "still held".
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);
    let up = RawControlEvent::DivertedButtons([0, 0, 0, 0]);

    handle_reprog(&mut acc, down, &[dpi], false, &tx);
    handle_reprog(&mut acc, up, &[dpi], false, &tx);
    handle_reprog(&mut acc, down, &[dpi], false, &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle))
    );
    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::ButtonPressed(ButtonId::DpiToggle)),
        "a release re-arms the rising edge"
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn dpi_gesture_mode_swipes_independently_of_gesture_button() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let dpi_down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);

    // DPI hold + swipe right → DpiToggle gesture, not a plain press.
    handle_reprog(&mut acc, dpi_down, &[dpi], true, &tx);
    acc.dpi_swipe.backdate_hold_for_test();
    handle_reprog(
        &mut acc,
        RawControlEvent::RawXy { dx: 120, dy: 5 },
        &[dpi],
        true,
        &tx,
    );

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture {
            button: ButtonId::DpiToggle,
            direction: GestureDirection::Right,
        })
    );
    assert!(rx.try_recv().is_err(), "no plain press in gesture mode");

    // Release after a committed swipe must not also click.
    handle_reprog(&mut acc, release(), &[dpi], true, &tx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn dpi_gesture_mode_quick_tap_is_a_click() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let dpi_down = RawControlEvent::DivertedButtons([dpi, 0, 0, 0]);

    handle_reprog(&mut acc, dpi_down, &[dpi], true, &tx);
    handle_reprog(&mut acc, release(), &[dpi], true, &tx);

    assert_eq!(
        rx.try_recv(),
        Ok(CapturedInput::Gesture {
            button: ButtonId::DpiToggle,
            direction: GestureDirection::Click,
        })
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn both_gesture_sources_can_commit_from_the_same_raw_xy_stream() {
    // When both controls are held (rare, but the paths must not steal each
    // other's accumulator), each swipe map commits independently.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut acc = CaptureAccum::default();
    let dpi = reprog_controls::DPI_MODE_SHIFT_CIDS[0];
    let both = RawControlEvent::DivertedButtons([reprog_controls::GESTURE_BUTTON_CID, dpi, 0, 0]);

    handle_reprog(&mut acc, both, &[dpi], true, &tx);
    acc.gesture_swipe.backdate_hold_for_test();
    acc.dpi_swipe.backdate_hold_for_test();
    handle_reprog(
        &mut acc,
        RawControlEvent::RawXy { dx: 0, dy: -120 },
        &[dpi],
        true,
        &tx,
    );

    let a = rx.try_recv().expect("first swipe");
    let b = rx.try_recv().expect("second swipe");
    let mut got = [a, b];
    got.sort_by_key(|e| match e {
        CapturedInput::Gesture { button, .. } => *button,
        _ => ButtonId::LeftClick,
    });
    assert_eq!(
        got,
        [
            CapturedInput::Gesture {
                button: ButtonId::DpiToggle,
                direction: GestureDirection::Up,
            },
            CapturedInput::Gesture {
                button: ButtonId::GestureButton,
                direction: GestureDirection::Up,
            },
        ]
    );
    assert!(rx.try_recv().is_err());
}

// ── Transactional arm bookkeeping (K8a) ────────────────────────────────────

#[derive(Default)]
struct MockDivert {
    /// CIDs currently diverted (true = raw_xy).
    live: std::collections::BTreeMap<u16, bool>,
    /// Divert calls that should fail.
    fail_on: std::collections::BTreeSet<u16>,
    /// Ordered log of operations for assertions.
    ops: Vec<&'static str>,
}

impl DivertBackend for MockDivert {
    fn divert(&mut self, cid: u16, raw_xy: bool) -> Result<(), String> {
        if self.fail_on.contains(&cid) {
            self.ops.push("fail");
            return Err(format!("inject fail cid={cid:#06x}"));
        }
        self.live.insert(cid, raw_xy);
        self.ops.push("divert");
        Ok(())
    }

    fn restore(&mut self, cid: u16) -> Result<(), String> {
        self.live.remove(&cid);
        self.ops.push("restore");
        Ok(())
    }
}

#[test]
fn arm_transaction_succeeds_records_all_steps() {
    let mut backend = MockDivert::default();
    let steps = [(0x00c3, true), (0x00c4, true)];
    let progress = apply_diverts_transactional(&mut backend, &steps).expect("arm");
    assert_eq!(progress.armed.len(), 2);
    assert_eq!(backend.live.len(), 2);
    assert_eq!(backend.live.get(&0x00c3), Some(&true));
    assert_eq!(backend.live.get(&0x00c4), Some(&true));
}

#[test]
fn arm_transaction_rolls_back_prior_on_second_fail() {
    let mut backend = MockDivert {
        fail_on: [0x00c4].into_iter().collect(),
        ..MockDivert::default()
    };
    let steps = [(0x00c3, true), (0x00c4, true)];
    let err = apply_diverts_transactional(&mut backend, &steps).expect_err("must fail");
    assert!(err.contains("00c4") || err.contains("c4"), "got: {err}");
    assert!(
        backend.live.is_empty(),
        "prior divert must be restored: {:?}",
        backend.live
    );
    assert_eq!(
        backend.ops,
        ["divert", "fail", "restore"],
        "GB divert, DPI fail, GB restore"
    );
}

#[test]
fn arm_transaction_rolls_back_two_priors_on_third_fail() {
    let mut backend = MockDivert {
        fail_on: [0x00c5].into_iter().collect(),
        ..MockDivert::default()
    };
    let steps = [(0x00c3, true), (0x00c4, false), (0x00c5, true)];
    let err = apply_diverts_transactional(&mut backend, &steps).expect_err("must fail");
    assert!(err.contains("c5"), "got: {err}");
    assert!(backend.live.is_empty());
    // Rollback is reverse order: c4 then c3.
    assert_eq!(
        backend.ops,
        ["divert", "divert", "fail", "restore", "restore"]
    );
}

#[test]
fn arm_progress_rollback_order_is_lifo() {
    let mut p = ArmProgress::default();
    p.record(1, true);
    p.record(2, false);
    p.record(3, true);
    let order: Vec<u16> = p.rollback_order().map(|d| d.cid).collect();
    assert_eq!(order, vec![3, 2, 1]);
    let armed: Vec<u16> = p.armed.iter().map(|d| d.cid).collect();
    assert_eq!(armed, vec![1, 2, 3]);
}
