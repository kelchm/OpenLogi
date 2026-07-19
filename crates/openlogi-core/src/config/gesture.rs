//! Multi-button gesture live-set types, load migration (schema v4), and the
//! canonical per-button resolver.
//!
//! The live set is an opaque [`GestureButtons`] value: either explicit `Off` or a
//! non-empty set of gesture-eligible [`ButtonId`]s. Dispatch and the GUI both
//! consult [`resolve_gesture_button`] so chips, arming, and maps cannot diverge.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::settings::GestureOwner;
use crate::binding::{
    Action, Binding, ButtonId, GestureDirection, default_binding_for, default_gesture_binding,
};

/// Reserved experimental multi-gesture field names. When any of these appears
/// on a device table under `schema_version >= 4`, load is rejected so we never
/// accept-and-discard an unknown multi-gesture layout.
pub const EXPERIMENTAL_GESTURE_MARKERS: &[&str] = &[
    "gesture_owners",
    "gesture_sources",
    "gesture_buttons_v2",
    "multi_gesture",
    "extra_gesture_buttons",
];

/// Whether `id` may appear in a live gesture multi-set.
#[must_use]
pub fn is_gesture_eligible(id: ButtonId) -> bool {
    matches!(
        id,
        ButtonId::GestureButton
            | ButtonId::DpiToggle
            | ButtonId::MiddleClick
            | ButtonId::Back
            | ButtonId::Forward
    )
}

/// Normalized live gesture-button set for one device.
///
/// Not publicly constructible as an empty set: constructors filter ineligible
/// ids and collapse the empty remainder to [`Self::off`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GestureButtons {
    kind: GestureButtonsKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GestureButtonsKind {
    Off,
    /// Invariant: non-empty; every id is gesture-eligible.
    Set(BTreeSet<ButtonId>),
}

impl GestureButtons {
    /// Explicitly off — no live gesture button.
    #[must_use]
    pub fn off() -> Self {
        Self {
            kind: GestureButtonsKind::Off,
        }
    }

    /// Build from an iterator, dropping ineligible ids. Empty after filter → Off.
    #[must_use]
    pub fn try_from_iter(ids: impl IntoIterator<Item = ButtonId>) -> Self {
        let set: BTreeSet<ButtonId> = ids.into_iter().filter(|id| is_gesture_eligible(*id)).collect();
        if set.is_empty() {
            Self::off()
        } else {
            Self {
                kind: GestureButtonsKind::Set(set),
            }
        }
    }

    /// Whether this is the explicit Off value.
    #[must_use]
    pub fn is_off(&self) -> bool {
        matches!(self.kind, GestureButtonsKind::Off)
    }

    /// Iterate live button ids (empty when Off).
    pub fn iter(&self) -> impl Iterator<Item = ButtonId> + '_ {
        match &self.kind {
            GestureButtonsKind::Off => None.into_iter().flatten(),
            GestureButtonsKind::Set(set) => Some(set.iter().copied()).into_iter().flatten(),
        }
    }

    /// Whether `id` is in the live set.
    #[must_use]
    pub fn contains(&self, id: ButtonId) -> bool {
        match &self.kind {
            GestureButtonsKind::Off => false,
            GestureButtonsKind::Set(set) => set.contains(&id),
        }
    }

    /// Insert an eligible id. Ineligible ids are ignored. Off + insert → singleton.
    #[must_use]
    pub fn insert(self, id: ButtonId) -> Self {
        if !is_gesture_eligible(id) {
            return self;
        }
        match self.kind {
            GestureButtonsKind::Off => Self::try_from_iter([id]),
            GestureButtonsKind::Set(mut set) => {
                set.insert(id);
                Self {
                    kind: GestureButtonsKind::Set(set),
                }
            }
        }
    }

    /// Remove `id`. Empty after remove → Off.
    #[must_use]
    pub fn remove(self, id: ButtonId) -> Self {
        match self.kind {
            GestureButtonsKind::Off => Self::off(),
            GestureButtonsKind::Set(mut set) => {
                set.remove(&id);
                if set.is_empty() {
                    Self::off()
                } else {
                    Self {
                        kind: GestureButtonsKind::Set(set),
                    }
                }
            }
        }
    }

    /// Number of live buttons (0 when Off).
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.kind {
            GestureButtonsKind::Off => 0,
            GestureButtonsKind::Set(set) => set.len(),
        }
    }

    /// Whether the live set is empty (Off).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.is_off()
    }
}

impl Default for GestureButtons {
    fn default() -> Self {
        Self::off()
    }
}

impl fmt::Display for GestureButtons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_off() {
            return f.write_str("Off");
        }
        let mut first = true;
        for id in self.iter() {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{id:?}")?;
        }
        Ok(())
    }
}

impl Serialize for GestureButtons {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.kind {
            GestureButtonsKind::Off => serializer.serialize_str("Off"),
            GestureButtonsKind::Set(set) => {
                // Stable TOML array of ButtonId variant names.
                set.iter()
                    .copied()
                    .collect::<Vec<ButtonId>>()
                    .serialize(serializer)
            }
        }
    }
}

/// Field presence for `gesture_buttons` during deserialize.
///
/// Distinguishes key-absent (run fold/infer) from key-present-and-parsed
/// (including `"Off"` and arrays that filter to Off).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum GestureButtonsField {
    /// Key absent from TOML → fold/infer on load.
    #[default]
    Absent,
    /// Key present and successfully parsed.
    Present(GestureButtons),
}

/// Deserialize a present `gesture_buttons` value into [`GestureButtonsField::Present`].
///
/// Absent keys use [`Default`] → [`GestureButtonsField::Absent`] and never call this.
pub(super) fn deserialize_gesture_buttons_field<'de, D>(
    deserializer: D,
) -> Result<GestureButtonsField, D::Error>
where
    D: Deserializer<'de>,
{
    struct FieldVisitor;

    impl<'de> Visitor<'de> for FieldVisitor {
        type Value = GestureButtons;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str(r#""Off", a ButtonId name, or an array of ButtonId names"#)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            if v == "Off" {
                return Ok(GestureButtons::off());
            }
            let button = ButtonId::deserialize(de::value::StrDeserializer::<E>::new(v))?;
            if !is_gesture_eligible(button) {
                // Eligible filter → empty → Off (same as array of junk names).
                return Ok(GestureButtons::off());
            }
            Ok(GestureButtons::try_from_iter([button]))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut ids = Vec::new();
            while let Some(token) = seq.next_element::<String>()? {
                // Unknown / ineligible names are skipped; empty remainder → Off.
                if let Ok(id) = ButtonId::deserialize(de::value::StrDeserializer::<
                    de::value::Error,
                >::new(&token))
                    && is_gesture_eligible(id)
                {
                    ids.push(id);
                }
            }
            Ok(GestureButtons::try_from_iter(ids))
        }
    }

    Ok(GestureButtonsField::Present(
        deserializer.deserialize_any(FieldVisitor)?,
    ))
}

/// Resolved state of one button for agent + GUI (canonical, K4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GestureButtonState {
    /// Not in the live set; may still store a preserved Gesture map.
    NotLive {
        /// Preserved map when the binding is still `Gesture` (non-destructive demotion).
        preserved: Option<BTreeMap<GestureDirection, Action>>,
    },
    /// In the live set but binding is Single (or absent after a hand-edit race).
    LiveButSingle {
        /// The single action (or [`Action::None`] when absent).
        action: Action,
    },
    /// In the live set with a (possibly sparse) Gesture map.
    LiveMap {
        /// Stored direction map; missing keys are no-ops at runtime.
        map: BTreeMap<GestureDirection, Action>,
    },
}

/// Mirror of master `infer_gesture_owner` — pure sole-seed used by migration.
#[must_use]
pub(super) fn master_infer_owner(bindings: &BTreeMap<ButtonId, Binding>) -> Option<ButtonId> {
    // First non-GestureButton button with Binding::Gesture in ButtonId declaration order.
    if let Some((id, _)) = bindings
        .iter()
        .find(|(id, b)| **id != ButtonId::GestureButton && b.is_gesture())
    {
        return Some(*id);
    }
    if matches!(
        bindings.get(&ButtonId::GestureButton),
        Some(Binding::Single(_))
    ) {
        return None;
    }
    Some(ButtonId::GestureButton)
}

fn is_gesture_map(bindings: &BTreeMap<ButtonId, Binding>, id: ButtonId) -> bool {
    matches!(bindings.get(&id), Some(Binding::Gesture(_)))
}

fn multi_union_oshook(base: GestureButtons, bindings: &BTreeMap<ButtonId, Binding>) -> GestureButtons {
    let mut out = base;
    for id in [ButtonId::MiddleClick, ButtonId::Back, ButtonId::Forward] {
        if is_gesture_map(bindings, id) {
            out = out.insert(id);
        }
    }
    out
}

/// Load normalize: drop live ids whose binding is Single/absent (except vacant
/// GestureButton, which materializes the main default five-pack as Custom).
pub(super) fn normalize_live_set(
    base: &GestureButtons,
    bindings: &mut BTreeMap<ButtonId, Binding>,
) -> GestureButtons {
    let mut out = GestureButtons::off();
    for id in base.iter() {
        match bindings.get(&id) {
            Some(Binding::Gesture(_)) => {
                out = out.insert(id);
            }
            None if id == ButtonId::GestureButton => {
                bindings.insert(
                    ButtonId::GestureButton,
                    default_binding_for(ButtonId::GestureButton),
                );
                out = out.insert(id);
            }
            // Single: drop from live set; keep the Single in storage.
            // Other vacant ids: drop (do not invent maps).
            Some(Binding::Single(_)) | None => {}
        }
    }
    out
}

/// Pure migrate algorithm (K6b + K6a). Evaluates ExplicitButtons / FoldOwner / Infer.
pub(super) fn migrate_live_set(
    source: &GestureButtonsField,
    gesture_owner: Option<GestureOwner>,
    bindings: &mut BTreeMap<ButtonId, Binding>,
) -> GestureButtons {
    match source {
        GestureButtonsField::Present(present) => {
            // ExplicitButtons: trust the file; no K6a promote; demotion sticky.
            normalize_live_set(present, bindings)
        }
        GestureButtonsField::Absent => match gesture_owner {
            Some(GestureOwner::Off) => GestureButtons::off(),
            Some(GestureOwner::Button(id)) if is_gesture_eligible(id) => {
                let mut base = GestureButtons::try_from_iter([id]);
                base = multi_union_oshook(base, bindings);
                // K6a: dual only when the *folded owner* is GestureButton.
                if id == ButtonId::GestureButton && is_gesture_map(bindings, ButtonId::DpiToggle) {
                    base = base.insert(ButtonId::DpiToggle);
                }
                normalize_live_set(&base, bindings)
            }
            Some(GestureOwner::Button(_)) => {
                // Ineligible owner token → Off.
                GestureButtons::off()
            }
            None => {
                // Infer: master sole seed → multi OS-hook union → K6a dual.
                let mut base = match master_infer_owner(bindings) {
                    None => GestureButtons::off(),
                    Some(id) => GestureButtons::try_from_iter([id]),
                };
                base = multi_union_oshook(base, bindings);
                if is_gesture_map(bindings, ButtonId::DpiToggle)
                    && master_infer_owner(bindings) == Some(ButtonId::GestureButton)
                {
                    base = base.insert(ButtonId::DpiToggle);
                }
                normalize_live_set(&base, bindings)
            }
        },
    }
}

/// First-enable map for a vacant or Single-bound button (K5b, without preset tags).
///
/// * Gesture Button vacant → main `default_gesture_binding` five-pack.
/// * Other vacant → full five keys of [`Action::None`].
/// * Prior Single → Click = that action, other dirs `None`.
/// * Existing Gesture → left unchanged.
pub(super) fn ensure_gesture_map_for_enable(
    bindings: &mut BTreeMap<ButtonId, Binding>,
    button: ButtonId,
) {
    match bindings.get(&button) {
        Some(Binding::Gesture(_)) => {}
        Some(Binding::Single(action)) => {
            let click = action.clone();
            let map = GestureDirection::ALL
                .into_iter()
                .map(|dir| {
                    let action = if dir == GestureDirection::Click {
                        click.clone()
                    } else {
                        Action::None
                    };
                    (dir, action)
                })
                .collect();
            bindings.insert(button, Binding::Gesture(map));
        }
        None if button == ButtonId::GestureButton => {
            bindings.insert(button, default_binding_for(ButtonId::GestureButton));
        }
        None => {
            let map = GestureDirection::ALL
                .into_iter()
                .map(|dir| (dir, Action::None))
                .collect();
            bindings.insert(button, Binding::Gesture(map));
        }
    }
}

/// Materialize the main default five-pack (same actions as `default_gesture_binding`).
#[must_use]
pub fn main_default_gesture_map() -> BTreeMap<GestureDirection, Action> {
    GestureDirection::ALL
        .into_iter()
        .map(|d| (d, default_gesture_binding(d)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "expect/unwrap are idiomatic in tests")]
mod tests {
    use super::*;

    #[test]
    fn try_from_iter_filters_ineligible_and_empty_to_off() {
        assert!(GestureButtons::try_from_iter([]).is_off());
        assert!(GestureButtons::try_from_iter([ButtonId::LeftClick]).is_off());
        let dual = GestureButtons::try_from_iter([
            ButtonId::GestureButton,
            ButtonId::DpiToggle,
            ButtonId::LeftClick,
        ]);
        assert!(dual.contains(ButtonId::GestureButton));
        assert!(dual.contains(ButtonId::DpiToggle));
        assert!(!dual.contains(ButtonId::LeftClick));
        assert_eq!(dual.len(), 2);
    }

    #[test]
    fn insert_remove_collapse_to_off() {
        let gb = GestureButtons::off().insert(ButtonId::Back);
        assert!(gb.contains(ButtonId::Back));
        let off = gb.remove(ButtonId::Back);
        assert!(off.is_off());
    }

    #[test]
    fn serialize_off_and_array() {
        // TOML documents need a table root — wrap the field like DeviceConfig does.
        #[derive(Serialize)]
        struct Wrap {
            gesture_buttons: GestureButtons,
        }
        let off = toml::to_string_pretty(&Wrap {
            gesture_buttons: GestureButtons::off(),
        })
        .expect("ser");
        assert!(off.contains("gesture_buttons = \"Off\""), "got: {off}");

        let set = GestureButtons::try_from_iter([ButtonId::GestureButton, ButtonId::DpiToggle]);
        let body = toml::to_string_pretty(&Wrap {
            gesture_buttons: set,
        })
        .expect("ser");
        assert!(
            body.contains("GestureButton") && body.contains("DpiToggle"),
            "got: {body}"
        );
        assert!(!body.contains("[]"), "empty array must never serialize");
    }

    #[test]
    fn master_infer_prefers_first_non_gb_gesture() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            ButtonId::GestureButton,
            Binding::Gesture(main_default_gesture_map()),
        );
        bindings.insert(
            ButtonId::DpiToggle,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::VolumeUp)])),
        );
        assert_eq!(
            master_infer_owner(&bindings),
            Some(ButtonId::DpiToggle),
            "DpiToggle is first non-GB Gesture in declaration order among configured"
        );

        bindings.insert(
            ButtonId::Back,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Copy)])),
        );
        assert_eq!(master_infer_owner(&bindings), Some(ButtonId::Back));
    }

    #[test]
    fn migrate_infer_dpi_only_is_singleton_dpi() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            ButtonId::DpiToggle,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::VolumeUp)])),
        );
        let live = migrate_live_set(&GestureButtonsField::Absent, None, &mut bindings);
        assert!(live.contains(ButtonId::DpiToggle));
        assert!(!live.contains(ButtonId::GestureButton));
    }

    #[test]
    fn migrate_fold_owner_gb_plus_dpi_gesture_is_dual() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            ButtonId::DpiToggle,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::VolumeUp)])),
        );
        let live = migrate_live_set(
            &GestureButtonsField::Absent,
            Some(GestureOwner::Button(ButtonId::GestureButton)),
            &mut bindings,
        );
        assert!(live.contains(ButtonId::GestureButton));
        assert!(live.contains(ButtonId::DpiToggle));
        // Vacant GB was materialized to main pack.
        assert!(matches!(
            bindings.get(&ButtonId::GestureButton),
            Some(Binding::Gesture(_))
        ));
    }

    #[test]
    fn migrate_explicit_buttons_sticky_no_promote() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            ButtonId::GestureButton,
            Binding::Gesture(main_default_gesture_map()),
        );
        bindings.insert(
            ButtonId::DpiToggle,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::VolumeUp)])),
        );
        let live = migrate_live_set(
            &GestureButtonsField::Present(GestureButtons::try_from_iter([ButtonId::GestureButton])),
            None,
            &mut bindings,
        );
        assert!(live.contains(ButtonId::GestureButton));
        assert!(
            !live.contains(ButtonId::DpiToggle),
            "ExplicitButtons must not K6a-promote"
        );
    }

    #[test]
    fn migrate_infer_back_and_forward_unions() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            ButtonId::Back,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Copy)])),
        );
        bindings.insert(
            ButtonId::Forward,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Paste)])),
        );
        let live = migrate_live_set(&GestureButtonsField::Absent, None, &mut bindings);
        assert!(live.contains(ButtonId::Back));
        assert!(live.contains(ButtonId::Forward));
        assert!(!live.contains(ButtonId::GestureButton));
    }

    #[test]
    fn migrate_normalize_drops_single_from_live_set() {
        let mut bindings = BTreeMap::new();
        bindings.insert(ButtonId::Back, Binding::Single(Action::BrowserBack));
        let live = migrate_live_set(
            &GestureButtonsField::Present(GestureButtons::try_from_iter([ButtonId::Back])),
            None,
            &mut bindings,
        );
        assert!(live.is_off());
        assert_eq!(
            bindings.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
    }
}
