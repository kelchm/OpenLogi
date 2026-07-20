//! Binding-map construction: overlay the stored per-device (and per-app)
//! bindings on top of the built-in defaults.
//!
//! Keyed by `config_key` (`Option<&str>`) rather than any UI device record so
//! both the agent and the GUI can build the effective map from a [`Config`].

use std::collections::BTreeMap;

use openlogi_core::binding::{
    Action, Binding, ButtonId, GestureDirection, default_binding, default_gesture_binding,
};
use openlogi_core::config::Config;

/// Effective per-button single-action map for the device `config_key`, with
/// `app_bundle`'s per-app overlay applied. Unset buttons fall back to
/// [`default_binding`].
///
/// This is the map the OS hook and the HID++ button-press path consume, so a
/// `Binding::Gesture` is projected to its `click_action()` — the gesture
/// button's per-direction swipes are dispatched via the separate
/// [`gesture_bindings_for`] map, not here.
#[must_use]
pub fn bindings_for(
    config: &Config,
    config_key: Option<&str>,
    app_bundle: Option<&str>,
) -> BTreeMap<ButtonId, Action> {
    let stored = config_key
        .map(|key| config.effective_bindings(key, app_bundle))
        .unwrap_or_default();
    let mut bindings: BTreeMap<ButtonId, Action> = ButtonId::ALL
        .iter()
        .copied()
        .map(|b| (b, default_binding(b)))
        .collect();
    for (k, binding) in stored {
        // A gesture binding with no explicit `Click` has no opinion on the
        // plain-press action, so leave the button's default seed in place rather
        // than clobbering it with the `Action::None` that `click_action()` would
        // project. (An explicit `Single(Action::None)` — a user-disabled button —
        // still overrides, as it should.)
        if binding.is_gesture() && binding.direction_action(GestureDirection::Click).is_none() {
            continue;
        }
        bindings.insert(k, binding.click_action());
    }
    bindings
}

/// Effective gesture bindings for the device `config_key`. Unset directions
/// fall back to [`default_gesture_binding`].
#[must_use]
pub fn gesture_bindings_for(
    config: &Config,
    config_key: Option<&str>,
) -> BTreeMap<GestureDirection, Action> {
    // The dedicated HID++ gesture button (CID 0x00c3) only gestures while it is the device's gesture
    // owner. When the user moves the role to an OS-hook button (Middle/Back/
    // Forward) or turns gestures off, return an empty map so the gesture watcher
    // dispatches nothing — otherwise the always-seeded defaults would keep the
    // HID++ gesture button firing regardless of the selection.
    let owner = config_key.and_then(|key| config.gesture_owner(key));
    if owner != Some(ButtonId::GestureButton) {
        return BTreeMap::new();
    }
    let stored = config_key
        .map(|key| config.gesture_bindings_for(key))
        .unwrap_or_default();
    let mut bindings: BTreeMap<GestureDirection, Action> = GestureDirection::ALL
        .iter()
        .copied()
        .map(|d| (d, default_gesture_binding(d)))
        .collect();
    for (k, v) in stored {
        bindings.insert(k, v);
    }
    bindings
}

/// Per-button direction maps for live HID++ gesture controls
/// ([`ButtonId::GestureButton`] and/or [`ButtonId::DpiToggle`]).
///
/// Built from [`Config::resolve_gesture_button`]: a button is included only when
/// it is in the live multi-set **and** stores a [`Binding::Gesture`] map. The
/// stored map is used as-is (sparse keys are no-ops at runtime) — projection
/// does not invent desktop defaults into DPI maps.
#[must_use]
pub fn hidpp_gestures_for(
    config: &Config,
    config_key: Option<&str>,
) -> BTreeMap<ButtonId, BTreeMap<GestureDirection, Action>> {
    use openlogi_core::config::GestureButtonState;

    let Some(key) = config_key else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for button in [ButtonId::GestureButton, ButtonId::DpiToggle] {
        if let GestureButtonState::LiveMap { map } = config.resolve_gesture_button(key, button) {
            out.insert(button, map);
        }
    }
    out
}

/// Per-direction maps for live OS-hook gesture buttons (Middle/Back/Forward)
/// on `config_key`, with `app_bundle`'s per-app overlay applied.
///
/// Includes every OS-hook id that is in the live multi-set and still a
/// [`Binding::Gesture`] after `effective_bindings` (a per-app Single demotes
/// that button for the foreground app only). Maps are stored-as-is (sparse =
/// no-op). Multi-hold among OS-hook buttons is optional PR2; PR1 still projects
/// multiple maps when migration produced them.
#[must_use]
pub fn oshook_gestures_for(
    config: &Config,
    config_key: Option<&str>,
    app_bundle: Option<&str>,
) -> BTreeMap<ButtonId, BTreeMap<GestureDirection, Action>> {
    let Some(key) = config_key else {
        return BTreeMap::new();
    };
    let live = config.gesture_buttons(key);
    let effective = config.effective_bindings(key, app_bundle);
    let mut out = BTreeMap::new();
    for id in [
        ButtonId::MiddleClick,
        ButtonId::Back,
        ButtonId::Forward,
    ] {
        if !live.contains(id) {
            continue;
        }
        if let Some(Binding::Gesture(map)) = effective.get(&id) {
            out.insert(id, map.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_less_gesture_keeps_default_click_in_projection() {
        // A gesture binding with no explicit `Click` (a migrated sparse v1 map or
        // a hand-edited config) must not project to `Action::None` and silently
        // disable the button — the button's default click survives.
        let mut cfg = Config::default();
        let mut map = BTreeMap::new();
        map.insert(GestureDirection::Up, Action::Copy);
        cfg.set_binding("2b042", ButtonId::GestureButton, Binding::Gesture(map));

        let projected = bindings_for(&cfg, Some("2b042"), None);
        assert_eq!(
            projected.get(&ButtonId::GestureButton),
            Some(&default_binding(ButtonId::GestureButton)),
            "a Click-less gesture must keep the default click, not None"
        );
    }

    #[test]
    fn explicit_gesture_click_overrides_default_in_projection() {
        // A gesture binding that DOES define `Click` projects that action.
        let mut cfg = Config::default();
        let mut map = BTreeMap::new();
        map.insert(GestureDirection::Click, Action::Paste);
        cfg.set_binding("2b042", ButtonId::GestureButton, Binding::Gesture(map));

        let projected = bindings_for(&cfg, Some("2b042"), None);
        assert_eq!(
            projected.get(&ButtonId::GestureButton),
            Some(&Action::Paste)
        );
    }

    #[test]
    fn oshook_gestures_collects_only_os_hook_gesture_buttons() {
        let mut cfg = Config::default();
        // A gesture-mode Back (an OS-hook button) — included, raw map preserved.
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Copy)])),
        );
        // A single-mode Middle — excluded (not a gesture button).
        cfg.set_binding("2b042", ButtonId::MiddleClick, Action::MiddleClick.into());
        // The dedicated HID++ gesture button — excluded (it never reaches the
        // OS hook, so it must not appear in the hook's gesture map).
        cfg.set_binding(
            "2b042",
            ButtonId::GestureButton,
            Binding::Gesture(BTreeMap::from([(
                GestureDirection::Up,
                Action::MissionControl,
            )])),
        );

        let oshook = oshook_gestures_for(&cfg, Some("2b042"), None);
        assert_eq!(oshook.len(), 1, "only the gesture-mode Back belongs here");
        assert_eq!(
            oshook.get(&ButtonId::Back),
            Some(&BTreeMap::from([(GestureDirection::Up, Action::Copy)]))
        );
        assert!(!oshook.contains_key(&ButtonId::MiddleClick));
        assert!(!oshook.contains_key(&ButtonId::GestureButton));
    }

    #[test]
    fn per_app_override_drops_the_owner_from_the_oshook_gesture_set() {
        // Back is the gesture owner globally...
        let mut cfg = Config::default();
        cfg.set_gesture_owner("2b042", ButtonId::Back);
        assert!(
            oshook_gestures_for(&cfg, Some("2b042"), None).contains_key(&ButtonId::Back),
            "Back gestures globally"
        );

        // ...but a per-app override makes it a single action in that app, so it
        // must drop out of the gesture set there (and fall through to the
        // single-action path, which applies the override).
        cfg.set_per_app_binding(
            "2b042",
            "com.apple.Safari",
            ButtonId::Back,
            Some(Action::NextTab),
        );
        assert!(
            oshook_gestures_for(&cfg, Some("2b042"), Some("com.apple.Safari")).is_empty(),
            "a per-app override of the owner removes it from the gesture set"
        );
        // Other apps are unaffected — Back still gestures.
        assert!(
            oshook_gestures_for(&cfg, Some("2b042"), Some("com.other.App"))
                .contains_key(&ButtonId::Back)
        );
    }

    #[test]
    fn gesture_bindings_silent_when_hidpp_button_is_not_the_owner() {
        let mut cfg = Config::default();
        // Default device: the dedicated HID++ gesture button owns gestures, so its defaults are seeded.
        let defaults = gesture_bindings_for(&cfg, Some("2b042"));
        assert_eq!(
            defaults.get(&GestureDirection::Up),
            Some(&default_gesture_binding(GestureDirection::Up)),
            "the default gesture owner is the dedicated HID++ gesture button"
        );

        // Move the gesture role to an OS-hook button: the HID++ gesture button goes silent,
        // so the watcher dispatches nothing for 0x00c3.
        cfg.set_gesture_owner("2b042", ButtonId::Back);
        assert!(
            gesture_bindings_for(&cfg, Some("2b042")).is_empty(),
            "HID++ gesture button must dispatch nothing once another button owns gestures"
        );
    }

    #[test]
    fn hidpp_gestures_includes_dpi_map_alongside_primary_owner() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.enable_gesture_button("2b042", ButtonId::DpiToggle);
        cfg.set_gesture_preset(
            "2b042",
            ButtonId::DpiToggle,
            openlogi_core::config::GesturePreset::MediaControls,
        );

        let maps = hidpp_gestures_for(&cfg, Some("2b042"));
        assert!(
            maps.contains_key(&ButtonId::GestureButton),
            "primary gesture button stays live"
        );
        assert_eq!(
            maps.get(&ButtonId::DpiToggle)
                .and_then(|m| m.get(&GestureDirection::Up)),
            Some(&Action::VolumeUp),
            "DPI gesture map is an independent second source"
        );
    }

    #[test]
    fn hidpp_gestures_omits_dpi_when_it_is_a_single_action() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.set_binding(
            "2b042",
            ButtonId::DpiToggle,
            Action::CycleDpiPresets.into(),
        );

        let maps = hidpp_gestures_for(&cfg, Some("2b042"));
        assert!(maps.contains_key(&ButtonId::GestureButton));
        assert!(
            !maps.contains_key(&ButtonId::DpiToggle),
            "a single-action DPI button is not a gesture source"
        );
    }

    #[test]
    fn hidpp_gestures_omits_preserved_not_live_dpi_map() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.enable_gesture_button("2b042", ButtonId::DpiToggle);
        cfg.set_gesture_preset(
            "2b042",
            ButtonId::DpiToggle,
            openlogi_core::config::GesturePreset::MediaControls,
        );
        cfg.disable_gesture_button("2b042", ButtonId::DpiToggle);

        let maps = hidpp_gestures_for(&cfg, Some("2b042"));
        assert!(maps.contains_key(&ButtonId::GestureButton));
        assert!(
            !maps.contains_key(&ButtonId::DpiToggle),
            "demoted DPI must not arm even though its map is preserved"
        );
    }

    #[test]
    fn oshook_gestures_uses_live_set_not_sole_owner_only() {
        let mut cfg = Config::default();
        // Explicit multi: Back live (via enable) with GB also live.
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.enable_gesture_button("2b042", ButtonId::Back);
        let oshook = oshook_gestures_for(&cfg, Some("2b042"), None);
        assert!(oshook.contains_key(&ButtonId::Back));
        assert!(!oshook.contains_key(&ButtonId::GestureButton));
    }

    #[test]
    fn hidpp_gestures_defaults_for_unconfigured_device() {
        // No device stanza: live defaults to {GestureButton} with main pack so a
        // fresh/headless agent still arms the dedicated pad.
        let cfg = Config::default();
        let maps = hidpp_gestures_for(&cfg, Some("never-seen"));
        assert!(maps.contains_key(&ButtonId::GestureButton));
        assert_eq!(
            maps.get(&ButtonId::GestureButton)
                .and_then(|m| m.get(&GestureDirection::Up)),
            Some(&default_gesture_binding(GestureDirection::Up))
        );
        assert!(!maps.contains_key(&ButtonId::DpiToggle));
    }
}
