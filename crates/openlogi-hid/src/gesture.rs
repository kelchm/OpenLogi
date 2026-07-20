//! Live control capture for one device: divert the MX dedicated gesture button, the
//! DPI/ModeShift button, and the thumb wheel over HID++ and turn their events
//! into [`CapturedInput`] the GUI can dispatch.
//!
//! [`run_capture_session`] holds a single HID++ channel open for one device,
//! enables diversion on whichever of those controls it exposes, registers one
//! message listener, and restores every control's default mapping on shutdown.
//! Using one channel matters: a second channel to the same device would split
//! its input-report stream, so all captured controls share this session.
//!
//! The session is transport-only — it has no opinion on what an input *does*.
//! The GUI maps each [`CapturedInput`] to the user's bound action and dispatches
//! it, mirroring how the CGEventTap hook handles the side buttons. The thumb
//! wheel is special: diverting it stops native horizontal scroll, so the GUI
//! re-synthesises scroll from the [`CapturedInput::Scroll`] deltas — the wheel
//! is therefore only diverted when its click is actually bound.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use hidpp::{channel::HidppChannel, device::Device, protocol::v20};
use openlogi_core::binding::{ButtonId, GestureDirection, SwipeAccumulator};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::reprog_controls::{self, RawControlEvent, ReprogControlsV4};
use crate::route::{DeviceRoute, open_route_channel};
use crate::thumbwheel::{self, Thumbwheel};
use crate::write::SharedChannel;

/// Shared slot holding the active capture session's open channel, so DPI /
/// SmartShift writes can reuse it instead of opening a fresh one. `None`
/// whenever no session is connected.
pub type CaptureChannel = Arc<RwLock<Option<SharedChannel>>>;

/// One input captured from the active device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapturedInput {
    /// A completed hold+swipe (or plain click) on a diverted HID++ control that
    /// is in gesture mode — the dedicated gesture button and/or the
    /// DPI/ModeShift button when that id is in the live HID++ gesture set.
    Gesture {
        /// Which physical control produced the gesture.
        button: ButtonId,
        /// Committed swipe direction, or [`GestureDirection::Click`] for a
        /// press that never crossed the swipe threshold.
        direction: GestureDirection,
    },
    /// A diverted button was pressed — the DPI/ModeShift button
    /// ([`ButtonId::DpiToggle`]) when it is *not* in gesture mode, or the
    /// thumb-wheel single tap ([`ButtonId::Thumbwheel`]).
    ButtonPressed(ButtonId),
    /// Thumb-wheel rotation to re-synthesise as horizontal scroll, in the
    /// wheel's `diverted_res` increments. Emitted only while the wheel is
    /// diverted to capture its click.
    Scroll(i16),
}

/// Why a capture session could not start (or had to stop).
#[derive(Debug, Error)]
pub enum GestureError {
    /// HID transport-level failure while enumerating or opening the device.
    #[error("HID transport error")]
    Hid(#[from] async_hid::HidError),
    /// No connected device matched the capture route.
    #[error("no connected device matched the capture route")]
    DeviceNotFound,
    /// The device at the target index did not answer HID++.
    #[error("device at index {0:#04x} did not respond to HID++")]
    DeviceUnreachable(u8),
    /// A HID++ feature call returned an error; inner string carries context.
    #[error("HID++ protocol error: {0}")]
    Hidpp(String),
}

/// Movement + button state accumulated across messages. Lives behind a `Mutex`
/// because the channel's read thread invokes the listener by shared reference.
#[derive(Default)]
struct CaptureAccum {
    /// Mid-swipe state for the diverted dedicated gesture button (raw-XY).
    gesture_swipe: SwipeAccumulator,
    /// Mid-swipe state for the DPI/ModeShift button when it is in gesture mode.
    dpi_swipe: SwipeAccumulator,
    /// Whether any DPI/ModeShift control was held in the last event — for
    /// rising-edge press detection when the DPI button is *not* gesturing.
    dpi_down: bool,
}

/// Capture the gesture button, DPI/ModeShift button, and (when
/// `capture_thumbwheel`) the thumb wheel on `route` until `shutdown` resolves,
/// forwarding each event to `sink`.
///
/// `hidpp_gesture_buttons` is the set of HID++ controls to arm for hold+swipe
/// (typically [`ButtonId::GestureButton`] and/or [`ButtonId::DpiToggle`]). Other
/// members are ignored. DPI is always diverted when present so its plain press
/// can be rebound; when it is also in the gesture set it is diverted with
/// raw-XY. Arming is **transactional**: any mid-arm failure restores
/// every control that was already diverted.
///
/// Opens and holds one HID++ channel, diverts whichever of those controls the
/// device exposes, and listens. Returns once `shutdown` fires (or its sender is
/// dropped), after restoring every diverted control. Setup errors are returned;
/// failures to restore on the way out are logged, not propagated.
pub async fn run_capture_session(
    route: DeviceRoute,
    capture_thumbwheel: bool,
    hidpp_gesture_buttons: BTreeSet<ButtonId>,
    sink: mpsc::UnboundedSender<CapturedInput>,
    shutdown: oneshot::Receiver<()>,
    channel_slot: CaptureChannel,
) -> Result<(), GestureError> {
    let chan = open_route_channel(&route)
        .await?
        .ok_or(GestureError::DeviceNotFound)?;
    let device_index = route.device_index();
    let armed = arm_controls(
        &chan,
        device_index,
        capture_thumbwheel,
        &hidpp_gesture_buttons,
    )
    .await?;

    // Publish this device's open channel so DPI/SmartShift writes reuse it
    // instead of opening their own. Cleared on the way out.
    if let Ok(mut slot) = channel_slot.write() {
        *slot = Some(SharedChannel::new(Arc::clone(&chan), route.clone()));
    }

    let accum = Arc::new(Mutex::new(CaptureAccum::default()));
    let reprog_index = armed.reprog.as_ref().map(|(_, idx)| *idx);
    let thumb_index = armed.thumb.as_ref().map(|(_, idx)| *idx);
    let dpi_set = armed.dpi_cids.clone();
    let dpi_gesture = armed.dpi_as_gesture;
    let listener = chan.add_msg_listener_guarded({
        let accum = Arc::clone(&accum);
        let sink = sink.clone();
        move |raw, matched| {
            if matched {
                return;
            }
            let msg = v20::Message::from(raw);
            if let Some(idx) = reprog_index
                && let Some(event) = reprog_controls::decode_event(&msg, device_index, idx)
            {
                // Recover the guard even if a prior holder panicked — the
                // critical section is panic-free, so the data is consistent.
                let mut acc = accum.lock().unwrap_or_else(PoisonError::into_inner);
                handle_reprog(&mut acc, event, &dpi_set, dpi_gesture, &sink);
                return;
            }
            if let Some(idx) = thumb_index
                && let Some(event) = thumbwheel::decode_event(&msg, device_index, idx)
            {
                if event.single_tap {
                    let _ = sink.send(CapturedInput::ButtonPressed(ButtonId::Thumbwheel));
                }
                if event.rotation != 0 {
                    let _ = sink.send(CapturedInput::Scroll(event.rotation));
                }
            }
        }
    });

    info!(
        index = device_index,
        gesture = armed.gesture_diverted,
        dpi_buttons = armed.dpi_cids.len(),
        dpi_as_gesture = armed.dpi_as_gesture,
        thumbwheel = armed.thumb.is_some(),
        "control capture active"
    );
    let _ = shutdown.await;

    drop(listener);
    if let Ok(mut slot) = channel_slot.write() {
        *slot = None;
    }
    armed.disarm().await;
    debug!(index = device_index, "control capture stopped");
    Ok(())
}

/// One successful divert recorded for transactional rollback / disarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmedDivert {
    cid: u16,
    /// Whether raw-XY was requested (informational; restore always clears both).
    raw_xy: bool,
}

/// Pure bookkeeping for transactional HID++ divert. Extracted so rollback
/// order can be unit-tested without a real device.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ArmProgress {
    armed: Vec<ArmedDivert>,
}

impl ArmProgress {
    fn record(&mut self, cid: u16, raw_xy: bool) {
        self.armed.push(ArmedDivert { cid, raw_xy });
    }

    /// Successful diverts in reverse order (last armed first on rollback).
    fn rollback_order(&self) -> impl Iterator<Item = ArmedDivert> + '_ {
        self.armed.iter().rev().copied()
    }

}

/// Test double / production seam for divert + restore of one control.
#[cfg(test)]
trait DivertBackend {
    /// Divert `cid` with optional raw-XY reporting.
    fn divert(&mut self, cid: u16, raw_xy: bool) -> Result<(), String>;
    /// Clear divert (and raw-XY) for `cid`.
    fn restore(&mut self, cid: u16) -> Result<(), String>;
}

/// Apply divert steps transactionally: on any failure, restore every prior
/// success (best-effort) and return the error.
#[cfg(test)]
fn apply_diverts_transactional(
    backend: &mut impl DivertBackend,
    steps: &[(u16, bool)],
) -> Result<ArmProgress, String> {
    let mut progress = ArmProgress::default();
    for &(cid, raw_xy) in steps {
        if let Err(e) = backend.divert(cid, raw_xy) {
            for prior in progress.rollback_order() {
                if let Err(re) = backend.restore(prior.cid) {
                    warn!(
                        cid = prior.cid,
                        error = %re,
                        "failed to restore control during arm rollback"
                    );
                }
            }
            return Err(e);
        }
        progress.record(cid, raw_xy);
    }
    Ok(progress)
}

/// The set of controls a session has diverted, kept so they can be handed back
/// to the firmware on teardown.
struct ArmedControls {
    /// `0x1b04` accessor + feature index, present when the device exposes it.
    reprog: Option<(ReprogControlsV4, u8)>,
    /// Whether the gesture button is diverted with raw-XY reporting.
    gesture_diverted: bool,
    /// DPI/ModeShift CIDs diverted (plain press or raw-XY gesture).
    dpi_cids: Vec<u16>,
    /// Whether diverted DPI CIDs report raw-XY (gesture mode).
    dpi_as_gesture: bool,
    /// `0x2150` accessor + feature index, present when the thumb wheel is
    /// diverted.
    thumb: Option<(Thumbwheel, u8)>,
}

impl ArmedControls {
    /// Restore every diverted control. Failures are logged, not propagated.
    async fn disarm(&self) {
        if let Some((rc, _)) = self.reprog.as_ref() {
            if self.gesture_diverted {
                let r = rc
                    .set_cid_reporting(reprog_controls::GESTURE_BUTTON_CID, false, false)
                    .await;
                restore(r, "gesture button");
            }
            for &cid in &self.dpi_cids {
                restore(rc.set_cid_reporting(cid, false, false).await, "DPI button");
            }
        }
        if let Some((tw, _)) = self.thumb.as_ref() {
            restore(tw.set_reporting(false, false).await, "thumb wheel");
        }
    }
}

/// Resolve features off the device's root and divert the controls we capture.
/// Arming of reprog CIDs is transactional.
async fn arm_controls(
    chan: &Arc<HidppChannel>,
    slot: u8,
    capture_thumbwheel: bool,
    hidpp_gesture_buttons: &BTreeSet<ButtonId>,
) -> Result<ArmedControls, GestureError> {
    let divert_gesture_button = hidpp_gesture_buttons.contains(&ButtonId::GestureButton);
    let dpi_as_gesture = hidpp_gesture_buttons.contains(&ButtonId::DpiToggle);

    let device = Device::new(Arc::clone(chan), slot)
        .await
        .map_err(|_| GestureError::DeviceUnreachable(slot))?;

    let (reprog, gesture_diverted, dpi_cids, dpi_gesture_armed) =
        arm_reprog_feature(&device, chan, slot, divert_gesture_button, dpi_as_gesture).await?;

    let thumb = arm_thumbwheel_feature(
        &device,
        chan,
        slot,
        capture_thumbwheel,
        reprog.as_ref(),
        gesture_diverted,
        &dpi_cids,
    )
    .await?;

    if !gesture_diverted && dpi_cids.is_empty() && thumb.is_none() {
        debug!(slot, "no capturable controls — idle session");
    }
    Ok(ArmedControls {
        reprog,
        gesture_diverted,
        dpi_cids,
        dpi_as_gesture: dpi_gesture_armed,
        thumb,
    })
}

/// Arm `0x1b04` controls transactionally. Returns (accessor, gesture flag, dpi cids, dpi raw-xy).
async fn arm_reprog_feature(
    device: &Device,
    chan: &Arc<HidppChannel>,
    slot: u8,
    divert_gesture_button: bool,
    dpi_as_gesture: bool,
) -> Result<(Option<(ReprogControlsV4, u8)>, bool, Vec<u16>, bool), GestureError> {
    let Some(info) = device
        .root()
        .get_feature(reprog_controls::FEATURE_ID)
        .await
        .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?
    else {
        return Ok((None, false, Vec::new(), false));
    };

    let rc = ReprogControlsV4::new(Arc::clone(chan), slot, info.index);
    let controls = enumerate_controls(&rc).await?;
    let steps = plan_reprog_diverts(&controls, divert_gesture_button, dpi_as_gesture);
    let progress = arm_reprog_steps(&rc, &steps).await?;

    let mut gesture_diverted = false;
    let mut dpi_cids = Vec::new();
    let mut dpi_gesture_armed = false;
    for d in &progress.armed {
        if d.cid == reprog_controls::GESTURE_BUTTON_CID {
            gesture_diverted = true;
        } else if reprog_controls::DPI_MODE_SHIFT_CIDS.contains(&d.cid) {
            dpi_cids.push(d.cid);
            if d.raw_xy {
                dpi_gesture_armed = true;
            }
        }
    }
    Ok((Some((rc, info.index)), gesture_diverted, dpi_cids, dpi_gesture_armed))
}

/// Build the ordered (cid, raw_xy) divert plan from the control table.
fn plan_reprog_diverts(
    controls: &[reprog_controls::CtrlIdInfo],
    divert_gesture_button: bool,
    dpi_as_gesture: bool,
) -> Vec<(u16, bool)> {
    let mut steps = Vec::new();
    if divert_gesture_button
        && let Some(control) = controls
            .iter()
            .find(|c| c.cid == reprog_controls::GESTURE_BUTTON_CID)
    {
        if control.supports_raw_xy() {
            steps.push((reprog_controls::GESTURE_BUTTON_CID, true));
        } else {
            debug!(
                cid = reprog_controls::GESTURE_BUTTON_CID,
                "gesture button lacks raw-XY — not diverting"
            );
        }
    }
    for &cid in &reprog_controls::DPI_MODE_SHIFT_CIDS {
        let Some(control) = controls.iter().find(|c| c.cid == cid) else {
            continue;
        };
        if !control.is_divertable() {
            continue;
        }
        let raw_xy = dpi_as_gesture && control.supports_raw_xy();
        if dpi_as_gesture && !control.supports_raw_xy() {
            debug!(cid, "DPI control lacks raw-XY — plain divert only");
        }
        steps.push((cid, raw_xy));
    }
    steps
}

/// Arm the thumb wheel after reprog. On failure, roll back every reprog divert.
async fn arm_thumbwheel_feature(
    device: &Device,
    chan: &Arc<HidppChannel>,
    slot: u8,
    capture_thumbwheel: bool,
    reprog: Option<&(ReprogControlsV4, u8)>,
    gesture_diverted: bool,
    dpi_cids: &[u16],
) -> Result<Option<(Thumbwheel, u8)>, GestureError> {
    if !capture_thumbwheel {
        return Ok(None);
    }
    let Some(info) = device
        .root()
        .get_feature(thumbwheel::FEATURE_ID)
        .await
        .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?
    else {
        return Ok(None);
    };

    let tw = Thumbwheel::new(Arc::clone(chan), slot, info.index);
    // Consume the getInfo error here, before the next await: Hidpp20Error
    // isn't Send, so holding it across an await would make this future
    // (spawned on tokio) non-Send.
    let supports_single_tap = match tw.get_info().await {
        Ok(twinfo) => twinfo.supports_single_tap,
        Err(e) => {
            warn!(error = ?e, "thumb wheel getInfo failed");
            false
        }
    };
    if !supports_single_tap {
        debug!("thumb wheel reports no single tap — click not capturable");
        return Ok(None);
    }
    if let Err(e) = tw.set_reporting(true, false).await {
        rollback_reprog(reprog, gesture_diverted, dpi_cids).await;
        return Err(GestureError::Hidpp(format!("{e:?}")));
    }
    Ok(Some((tw, info.index)))
}

/// Best-effort restore of every reprog divert (used when a later arm step fails).
async fn rollback_reprog(
    reprog: Option<&(ReprogControlsV4, u8)>,
    gesture_diverted: bool,
    dpi_cids: &[u16],
) {
    let Some((rc, _)) = reprog else {
        return;
    };
    if gesture_diverted {
        restore(
            rc.set_cid_reporting(reprog_controls::GESTURE_BUTTON_CID, false, false)
                .await,
            "gesture button (arm rollback)",
        );
    }
    for &cid in dpi_cids {
        restore(
            rc.set_cid_reporting(cid, false, false).await,
            "DPI button (arm rollback)",
        );
    }
}

/// Divert each reprog step in order; on failure restore prior successes.
async fn arm_reprog_steps(
    rc: &ReprogControlsV4,
    steps: &[(u16, bool)],
) -> Result<ArmProgress, GestureError> {
    let mut progress = ArmProgress::default();
    for &(cid, raw_xy) in steps {
        match rc.set_cid_reporting(cid, true, raw_xy).await {
            Ok(()) => progress.record(cid, raw_xy),
            Err(e) => {
                for prior in progress.rollback_order() {
                    restore(
                        rc.set_cid_reporting(prior.cid, false, false).await,
                        "reprog control (arm rollback)",
                    );
                }
                return Err(GestureError::Hidpp(format!("{e:?}")));
            }
        }
    }
    Ok(progress)
}

/// Log (don't propagate) a failure to hand a control back to the firmware.
fn restore<E: std::fmt::Display>(result: Result<(), E>, what: &str) {
    if let Err(e) = result {
        warn!(error = %e, control = what, "failed to restore control mapping on shutdown");
    }
}

/// Read the device's full reprogrammable-control table in one pass, so we can
/// test several CIDs without rescanning per control.
async fn enumerate_controls(
    rc: &ReprogControlsV4,
) -> Result<Vec<reprog_controls::CtrlIdInfo>, GestureError> {
    let count = rc
        .get_count()
        .await
        .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?;
    let mut controls = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        controls.push(
            rc.get_ctrl_id_info(index)
                .await
                .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?,
        );
    }
    Ok(controls)
}

/// Update `acc` and emit on a decoded `0x1b04` event: commit a gesture swipe the
/// instant it crosses the threshold (mid-swipe, like Options+) rather than on
/// release. The dedicated gesture button and (when `dpi_as_gesture`) the DPI
/// button each keep their own accumulator so both can gesture independently.
/// When the DPI button is *not* gesturing, emit a [`ButtonId::DpiToggle`] press
/// on the rising edge instead.
fn handle_reprog(
    acc: &mut CaptureAccum,
    event: RawControlEvent,
    dpi_cids: &[u16],
    dpi_as_gesture: bool,
    sink: &mpsc::UnboundedSender<CapturedInput>,
) {
    match event {
        RawControlEvent::DivertedButtons(cids) => {
            let gesture_held = cids.contains(&reprog_controls::GESTURE_BUTTON_CID);
            if gesture_held && !acc.gesture_swipe.is_holding() {
                acc.gesture_swipe.begin();
            } else if !gesture_held && acc.gesture_swipe.is_holding() {
                // A press that never committed a direction is a plain click.
                if acc.gesture_swipe.end() {
                    debug!("gesture click");
                    let _ = sink.send(CapturedInput::Gesture {
                        button: ButtonId::GestureButton,
                        direction: GestureDirection::Click,
                    });
                }
            }

            let dpi_held = dpi_cids.iter().any(|cid| cids.contains(cid));
            if dpi_as_gesture {
                if dpi_held && !acc.dpi_swipe.is_holding() {
                    acc.dpi_swipe.begin();
                } else if !dpi_held && acc.dpi_swipe.is_holding() && acc.dpi_swipe.end() {
                    debug!("dpi gesture click");
                    let _ = sink.send(CapturedInput::Gesture {
                        button: ButtonId::DpiToggle,
                        direction: GestureDirection::Click,
                    });
                }
            } else if dpi_held && !acc.dpi_down {
                let _ = sink.send(CapturedInput::ButtonPressed(ButtonId::DpiToggle));
            }
            acc.dpi_down = dpi_held;
        }
        RawControlEvent::RawXy { dx, dy } => {
            // Commit the instant a clean direction emerges (mid-swipe, once per
            // hold); the accumulator gates on hold duration internally and drops
            // travel that arrives outside a hold. Feed every currently-held
            // gesture-mode control so the two maps stay independent.
            if let Some(direction) = acc.gesture_swipe.accumulate(i32::from(dx), i32::from(dy)) {
                debug!(?direction, "gesture button swipe committed");
                let _ = sink.send(CapturedInput::Gesture {
                    button: ButtonId::GestureButton,
                    direction,
                });
            }
            if dpi_as_gesture
                && let Some(direction) = acc.dpi_swipe.accumulate(i32::from(dx), i32::from(dy))
            {
                debug!(?direction, "dpi button swipe committed");
                let _ = sink.send(CapturedInput::Gesture {
                    button: ButtonId::DpiToggle,
                    direction,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
