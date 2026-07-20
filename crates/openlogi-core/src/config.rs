//! User configuration, persisted as TOML at the platform-standard config
//! path.
//!
//! Per-device state (button bindings, …) lives under the
//! [`Config::devices`] map, keyed by a stable physical-device identifier such
//! as `"receiver:abc123:slot:2"`. Schema migrations branch on
//! [`Config::schema_version`].

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod device;
mod gesture;
mod settings;

pub use device::{DeviceConfig, DeviceIdentity};
pub use gesture::{
    EXPERIMENTAL_GESTURE_MARKERS, GestureButtonState, GestureButtons, GesturePreset,
    is_gesture_eligible, main_default_gesture_map, match_gesture_preset, media_controls_map,
    preset_table, window_navigation_map,
};
pub use settings::{
    AppSettings, Appearance, AssetSourcePreference, DEFAULT_THUMBWHEEL_SENSITIVITY, GestureOwner,
    Lighting, MAX_THUMBWHEEL_SENSITIVITY, MIN_THUMBWHEEL_SENSITIVITY,
    SMARTSHIFT_AUTO_DISENGAGE_DEFAULT, SMARTSHIFT_MIN_AUTO_DISENGAGE, ScrollResolution, SmartShift,
    WheelMode,
};

use crate::binding::{Action, Binding, ButtonId, GestureDirection, default_binding_for};
use crate::paths::{self, PathsError};
use gesture::{GestureButtonsField, ensure_gesture_map_for_enable, migrate_live_set};

/// The schema version the current build produces. Bumped on breaking layout
/// changes; readers branch on the parsed value before consuming the rest of
/// the file.
///
/// v4 replaces the sole `gesture_owner` scalar with a multi-set
/// `gesture_buttons` (`"Off"` or an array of eligible button names). Legacy
/// sole-owner files fold through [`migrate_live_set`]; experimental multi-
/// gesture layouts under v4+ are rejected rather than strip-and-rewrite.
///
/// v3 changes the device map from model keys to physical-device keys. No v2
/// device entries are migrated because model-scoped settings cannot be assigned
/// safely when two identical devices exist.
///
/// v2 merged the per-device `button_bindings` + `gesture_bindings` maps into a
/// single `bindings: BTreeMap<ButtonId, Binding>`. A v1 file still loads (the
/// `RawDeviceConfig` shim folds the legacy fields) and self-heals on the next
/// save; [`Config::load_from_path`] rejects only versions *newer* than this so a
/// forward file fails loudly instead of silently losing bindings.
pub const SCHEMA_VERSION: u32 = 4;

/// Top-level config document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Schema version the file was written with. Compared against
    /// [`SCHEMA_VERSION`] on load: older layouts migrate, newer ones are
    /// rejected loudly rather than silently losing settings.
    pub schema_version: u32,
    /// Non-device-scoped preferences (autostart, tray, language, …).
    #[serde(default, skip_serializing_if = "AppSettings::is_default")]
    pub app_settings: AppSettings,
    /// Physical config key of the carousel-selected device, persisted so a
    /// restart restores the last view rather than always landing on the
    /// first paired device. `None` means "fall back to the first device".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_device: Option<String>,
    /// Per-device state, keyed by the stable physical-device identifier
    /// (e.g. `"receiver:abc123:slot:2"`) so two identical models never share
    /// an entry.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            app_settings: AppSettings::default(),
            selected_device: None,
            devices: BTreeMap::new(),
        }
    }
}

/// Failure loading or persisting `config.toml`. The file-scoped variants
/// carry the offending path so callers can surface an actionable message.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The platform config directory could not be resolved (no home
    /// directory for the current user).
    #[error("could not resolve config path")]
    Path(#[from] PathsError),
    /// Reading the config file from disk failed.
    #[error("could not read config at {path}")]
    Read {
        /// The config file the read targeted.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The file was read but is not valid TOML for this schema.
    #[error("could not parse config at {path}")]
    Parse {
        /// The config file that failed to parse.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        #[source]
        source: toml::de::Error,
    },
    /// Writing the updated config back to disk failed.
    #[error("could not write config at {path}")]
    Write {
        /// The config file the write targeted.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The in-memory config could not be serialized to TOML — a bug in the
    /// config types rather than user error, since [`Config`] always
    /// serializes cleanly.
    #[error("could not serialize config")]
    Serialize(#[from] toml::ser::Error),
    /// The file declares a `schema_version` newer than this build
    /// understands; failing loudly avoids silently dropping settings a newer
    /// build wrote.
    #[error("config at {path} has unsupported schema_version {found}")]
    UnsupportedSchemaVersion {
        /// The config file carrying the unsupported version.
        path: PathBuf,
        /// The `schema_version` the file declared.
        found: u32,
    },
    /// The file uses a reserved experimental multi-gesture layout that this
    /// build does not understand. The file is left untouched — restore from
    /// backup or re-export from the build that wrote it.
    #[error(
        "config at {path} has unsupported experimental multi-gesture config \
         (device {device}, field `{marker}`); restore from backup or re-export \
         from the build that wrote it"
    )]
    UnsupportedExperimentalGesture {
        /// The config file carrying the experimental layout.
        path: PathBuf,
        /// Device map key under `devices`.
        device: String,
        /// The reserved experimental field name that triggered the reject.
        marker: String,
    },
}

#[allow(
    clippy::result_large_err,
    reason = "Config I/O keeps rich parse/write context and is not a hot path"
)]
impl Config {
    /// Loads the config from the default user path, returning
    /// [`Config::default`] if the file does not exist yet.
    pub fn load_or_default() -> Result<Self, ConfigError> {
        Self::load_from_path(&paths::config_path()?)
    }

    /// Same as [`Self::load_or_default`] but reads from `path`. Used by tests
    /// to avoid touching the real user config.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                // Peek the raw TOML for schema version + experimental markers
                // *before* accepting a deserialize that would strip unknown
                // multi-gesture fields. Reject without calling save.
                let raw: toml::Value =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let found_version = raw
                    .get("schema_version")
                    .and_then(toml::Value::as_integer)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0);
                if found_version > SCHEMA_VERSION {
                    return Err(ConfigError::UnsupportedSchemaVersion {
                        path: path.to_path_buf(),
                        found: found_version,
                    });
                }
                if found_version >= 4 {
                    reject_experimental_gesture_layout(&raw, path)?;
                }

                let mut config: Self =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: path.to_path_buf(),
                        source,
                    })?;
                // Accept any version up to the current one: older files migrate
                // through the per-device [`RawDeviceConfig`] shim and self-heal on
                // the next save. Only a *newer* file is rejected — loudly, so a
                // downgraded binary refuses to load (and silently wipe) a config
                // it can't represent.
                if config.schema_version > SCHEMA_VERSION {
                    return Err(ConfigError::UnsupportedSchemaVersion {
                        path: path.to_path_buf(),
                        found: config.schema_version,
                    });
                }
                // Stamp the in-memory doc to the current version so a re-save
                // writes the migrated shape (device shim already folded legacy
                // fields and stamped `gesture_buttons` during deserialize).
                config.schema_version = SCHEMA_VERSION;
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes the config atomically to the default user path: serialize to a
    /// sibling temp file, then rename over the target. On Unix the temp file
    /// is created with mode 0600.
    pub fn save_atomic(&self) -> Result<(), ConfigError> {
        self.save_to_path(&paths::config_path()?)
    }

    /// Same as [`Self::save_atomic`] but writes to `path`. Used by tests.
    pub fn save_to_path(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let body = toml::to_string_pretty(self)?;
        write_atomic(path, body.as_bytes()).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Returns the bindings stored for `device_key`, or an empty map if the
    /// device has no committed bindings yet.
    #[must_use]
    pub fn bindings_for(&self, device_key: &str) -> BTreeMap<ButtonId, Binding> {
        self.devices
            .get(device_key)
            .map(|d| d.bindings.clone())
            .unwrap_or_default()
    }

    /// Records `binding` for `button` on `device_key`, creating the device
    /// entry if needed. Replaces the whole binding (use
    /// [`Self::set_gesture_direction`] to edit one direction of a gesture
    /// binding in place).
    ///
    /// A non-gesture binding destroys the preset tag (K13) and drops the button
    /// from the live multi-set when present (non-destructive maps are only kept
    /// for demoted `Binding::Gesture` entries).
    pub fn set_binding(&mut self, device_key: &str, button: ButtonId, binding: Binding) {
        let device = self.devices.entry(device_key.to_string()).or_default();
        let is_gesture = binding.is_gesture();
        device.bindings.insert(button, binding);
        if !is_gesture {
            device.gesture_presets.remove(&button);
            if let Some(live) = device.gesture_buttons.take() {
                device.gesture_buttons = Some(live.remove(button));
            }
        }
    }

    /// Returns the gesture sub-bindings for `device_key`'s gesture button, or an
    /// empty map if it isn't in gesture mode. Derived from the unified
    /// [`DeviceConfig::bindings`]; kept as a convenience for the agent-side
    /// per-direction adapter.
    #[must_use]
    pub fn gesture_bindings_for(&self, device_key: &str) -> BTreeMap<GestureDirection, Action> {
        match self
            .devices
            .get(device_key)
            .and_then(|d| d.bindings.get(&ButtonId::GestureButton))
        {
            Some(Binding::Gesture(map)) => map.clone(),
            _ => BTreeMap::new(),
        }
    }

    /// Records `action` for one `direction` of `button`'s gesture binding,
    /// creating the device entry if needed.
    ///
    /// A button with no binding yet is seeded from its canonical
    /// [`default_binding_for`] — for [`ButtonId::GestureButton`] that is the full
    /// default direction map (including a [`GestureDirection::Click`]), so the
    /// merged map never persists a gesture binding whose click projection is a
    /// no-op. A prior [`Binding::Single`] is upgraded to [`Binding::Gesture`],
    /// preserving its action as the `Click` entry.
    pub fn set_gesture_direction(
        &mut self,
        device_key: &str,
        button: ButtonId,
        direction: GestureDirection,
        action: Action,
    ) {
        if let Binding::Gesture(map) = self.ensure_gesture_binding(device_key, button) {
            map.insert(direction, action);
        }
        // Editing any direction while a named preset is selected → Custom (K5).
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .gesture_presets
            .insert(button, GesturePreset::Custom);
    }

    /// Ensure `button` on `device_key` is a [`Binding::Gesture`], creating the
    /// device + a default binding if needed and upgrading a [`Binding::Single`]
    /// in place (its action kept as the [`GestureDirection::Click`]). Returns the
    /// entry so the caller can finish it — seed every direction or set just one.
    /// Shared by direction edits so promoting a button into gesture mode for a
    /// single-direction edit still produces a Gesture map.
    fn ensure_gesture_binding(&mut self, device_key: &str, button: ButtonId) -> &mut Binding {
        let entry = self
            .devices
            .entry(device_key.to_string())
            .or_default()
            .bindings
            .entry(button)
            .or_insert_with(|| default_binding_for(button));
        entry.upgrade_to_gesture();
        entry
    }

    /// Live multi-set of gesture buttons for `device_key`.
    ///
    /// * No device entry → default `{GestureButton}`.
    /// * Stamped `gesture_buttons` → that value.
    /// * Unstamped in-memory entry → pure Infer migrate (without writing).
    #[must_use]
    pub fn gesture_buttons(&self, device_key: &str) -> GestureButtons {
        let Some(device) = self.devices.get(device_key) else {
            return GestureButtons::try_from_iter([ButtonId::GestureButton]);
        };
        if let Some(gb) = &device.gesture_buttons {
            gb.clone()
        } else {
            let mut bindings = device.bindings.clone();
            migrate_live_set(&GestureButtonsField::Absent, None, &mut bindings)
        }
    }

    /// Canonical per-button gesture state for agent + GUI (K4).
    #[must_use]
    pub fn resolve_gesture_button(
        &self,
        device_key: &str,
        button: ButtonId,
    ) -> GestureButtonState {
        let live = self.gesture_buttons(device_key);
        let binding = self
            .devices
            .get(device_key)
            .and_then(|d| d.bindings.get(&button));
        match (live.contains(button), binding) {
            (false, Some(Binding::Gesture(map))) => GestureButtonState::NotLive {
                preserved: Some(map.clone()),
            },
            (false, _) => GestureButtonState::NotLive { preserved: None },
            (true, Some(Binding::Gesture(map))) => GestureButtonState::LiveMap {
                map: map.clone(),
            },
            (true, Some(Binding::Single(action))) => GestureButtonState::LiveButSingle {
                action: action.clone(),
            },
            // Vacant live Gesture Button (no device stanza / not yet stamped):
            // project the main default five-pack so a fresh device still arms and
            // dispatches OpenLogi defaults until enable/load materializes storage.
            (true, None) if button == ButtonId::GestureButton => GestureButtonState::LiveMap {
                map: main_default_gesture_map(),
            },
            (true, None) => GestureButtonState::LiveButSingle {
                action: Action::None,
            },
        }
    }

    /// Add `button` to the live multi-set (K5b first-enable map rules).
    ///
    /// Ineligible ids are ignored. Already-live buttons keep their maps; an
    /// unstamped infer default is still stamped so the next save is Explicit.
    pub fn enable_gesture_button(&mut self, device_key: &str, button: ButtonId) {
        if !is_gesture_eligible(button) {
            return;
        }
        let current = self.gesture_buttons(device_key);
        let next = if current.contains(button) {
            current
        } else {
            current.insert(button)
        };
        let device = self.devices.entry(device_key.to_string()).or_default();
        for id in next.iter() {
            let had_gesture = matches!(device.bindings.get(&id), Some(Binding::Gesture(_)));
            ensure_gesture_map_for_enable(&mut device.bindings, id);
            if had_gesture {
                // Keep a stored/derived named tag over an existing map.
                device
                    .gesture_presets
                    .entry(id)
                    .or_insert(GesturePreset::Custom);
            } else {
                // Newly materialised or upgraded from Single → Custom (K5b).
                // Must overwrite a stale named tag left after a Single rebind.
                device.gesture_presets.insert(id, GesturePreset::Custom);
            }
        }
        device.gesture_buttons = Some(next);
    }

    /// Remove `button` from the live multi-set. Empty → Off. Maps are preserved.
    pub fn disable_gesture_button(&mut self, device_key: &str, button: ButtonId) {
        let next = self.gesture_buttons(device_key).remove(button);
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .gesture_buttons = Some(next);
    }

    /// Replace the live multi-set with `buttons` (filtered to eligible). Empty → Off.
    ///
    /// Each newly enabled button gets a K5b first-enable map; already-Gesture
    /// maps are kept. Buttons leaving the set keep their maps (non-destructive).
    pub fn set_gesture_buttons(
        &mut self,
        device_key: &str,
        buttons: impl IntoIterator<Item = ButtonId>,
    ) {
        let desired = GestureButtons::try_from_iter(buttons);
        let device = self.devices.entry(device_key.to_string()).or_default();
        for id in desired.iter() {
            let had_gesture = matches!(device.bindings.get(&id), Some(Binding::Gesture(_)));
            ensure_gesture_map_for_enable(&mut device.bindings, id);
            if had_gesture {
                device
                    .gesture_presets
                    .entry(id)
                    .or_insert(GesturePreset::Custom);
            } else {
                device.gesture_presets.insert(id, GesturePreset::Custom);
            }
        }
        device.gesture_buttons = Some(desired);
    }

    /// Preset tag for `button` on `device_key` (K5 / K5a).
    ///
    /// Stored tag wins; otherwise derive from the Gesture map (exact table →
    /// named, else Custom). Non-gesture / absent → Custom.
    #[must_use]
    pub fn gesture_preset(&self, device_key: &str, button: ButtonId) -> GesturePreset {
        if let Some(tag) = self
            .devices
            .get(device_key)
            .and_then(|d| d.gesture_presets.get(&button))
            .copied()
        {
            return tag;
        }
        match self
            .devices
            .get(device_key)
            .and_then(|d| d.bindings.get(&button))
        {
            Some(Binding::Gesture(map)) => match_gesture_preset(map),
            _ => GesturePreset::Custom,
        }
    }

    /// Apply a preset selection (K5).
    ///
    /// * Named preset → replace the full five-map from the table and set the tag.
    /// * Custom → set the tag only (map is not wiped).
    ///
    /// Does not change live-set membership; pair with [`Self::enable_gesture_button`]
    /// when first enabling a button.
    pub fn set_gesture_preset(
        &mut self,
        device_key: &str,
        button: ButtonId,
        preset: GesturePreset,
    ) {
        let device = self.devices.entry(device_key.to_string()).or_default();
        if let Some(table) = preset_table(preset) {
            device.bindings.insert(button, Binding::Gesture(table));
        }
        // Custom with no prior Gesture map: leave bindings alone (caller may
        // enable first). Tag is always written.
        device.gesture_presets.insert(button, preset);
    }

    /// Compat sole-owner view of the multi-set (K14).
    ///
    /// Preference: first live OS-hook (Middle, Back, Forward) → GestureButton →
    /// DpiToggle. Empty / Off → `None`. Absent device → default GestureButton.
    #[must_use]
    pub fn gesture_owner(&self, device_key: &str) -> Option<ButtonId> {
        let live = self.gesture_buttons(device_key);
        if live.is_off() {
            return None;
        }
        for id in [
            ButtonId::MiddleClick,
            ButtonId::Back,
            ButtonId::Forward,
        ] {
            if live.contains(id) {
                return Some(id);
            }
        }
        if live.contains(ButtonId::GestureButton) {
            return Some(ButtonId::GestureButton);
        }
        if live.contains(ButtonId::DpiToggle) {
            return Some(ButtonId::DpiToggle);
        }
        None
    }

    /// Compat sole-owner setter (K14): when `button` is already the sole-owner
    /// shim result, membership is left unchanged (so a GUI re-click does not
    /// collapse a dual HID++ set) but the map is still ensured and the set is
    /// stamped. Otherwise replace the live set with the singleton `{button}`.
    pub fn set_gesture_owner(&mut self, device_key: &str, button: ButtonId) {
        if self.gesture_owner(device_key) == Some(button) {
            let live = self.gesture_buttons(device_key);
            let device = self.devices.entry(device_key.to_string()).or_default();
            ensure_gesture_map_for_enable(&mut device.bindings, button);
            if device.gesture_buttons.is_none() {
                device.gesture_buttons = Some(live);
            }
            return;
        }
        self.set_gesture_buttons(device_key, [button]);
    }

    /// Turn gestures off for `device_key`, recording explicit Off. Every button
    /// keeps its gesture map intact (nothing is destroyed).
    pub fn disable_gestures(&mut self, device_key: &str) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .gesture_buttons = Some(GestureButtons::off());
    }

    /// Resolve the effective binding map for `device_key`, overlaying the
    /// per-app entry for `bundle_id` (if any) on top of the global per-device
    /// `bindings`. A per-app override replaces the whole button with a
    /// [`Binding::Single`]; everything else falls through.
    ///
    /// Returns an empty map when the device has no recorded bindings yet.
    /// Callers (the GUI / hook) layer their own defaults on top.
    #[must_use]
    pub fn effective_bindings(
        &self,
        device_key: &str,
        bundle_id: Option<&str>,
    ) -> BTreeMap<ButtonId, Binding> {
        let Some(device) = self.devices.get(device_key) else {
            return BTreeMap::new();
        };
        let mut out = device.bindings.clone();
        if let Some(bid) = bundle_id
            && let Some(overlay) = device.per_app_bindings.get(bid)
        {
            for (k, v) in overlay {
                out.insert(*k, Binding::Single(v.clone()));
            }
        }
        out
    }

    /// Records a per-app override. Creates the device + app entries as
    /// needed; passing an action of `None` removes the override and prunes
    /// the empty app map.
    pub fn set_per_app_binding(
        &mut self,
        device_key: &str,
        bundle_id: &str,
        button: ButtonId,
        action: Option<Action>,
    ) {
        let entry = self
            .devices
            .entry(device_key.to_string())
            .or_default()
            .per_app_bindings
            .entry(bundle_id.to_string())
            .or_default();
        match action {
            Some(a) => {
                entry.insert(button, a);
            }
            None => {
                entry.remove(&button);
            }
        }
        if let Some(d) = self.devices.get_mut(device_key) {
            d.per_app_bindings.retain(|_, m| !m.is_empty());
        }
    }

    /// HID++ config key of the carousel-selected device, if any.
    #[must_use]
    pub fn selected_device(&self) -> Option<&str> {
        self.selected_device.as_deref()
    }

    /// Update the carousel-selected device. Pass `None` to clear the
    /// selection (e.g. when the previously-selected device disappears).
    pub fn set_selected_device(&mut self, key: Option<String>) {
        self.selected_device = key;
    }

    /// The ordered DPI preset list for `device_key`, or an empty `Vec` if the
    /// device has none configured yet.
    #[must_use]
    pub fn dpi_presets(&self, device_key: &str) -> Vec<u32> {
        self.devices
            .get(device_key)
            .map(|d| d.dpi_presets.clone())
            .unwrap_or_default()
    }

    /// Replace the DPI preset list for `device_key`. Pass an empty `Vec` to
    /// clear (the device block is kept; the field is just omitted on save
    /// thanks to `skip_serializing_if`).
    pub fn set_dpi_presets(&mut self, device_key: &str, presets: Vec<u32>) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .dpi_presets = presets;
    }

    /// The last-known [`DeviceIdentity`] for `device_key`, or `None` if the
    /// device has never been seen online (or was configured before identities
    /// were recorded).
    #[must_use]
    pub fn device_identity(&self, device_key: &str) -> Option<&DeviceIdentity> {
        self.devices
            .get(device_key)
            .and_then(|d| d.identity.as_ref())
    }

    /// Record (or refresh) the identity captured for `device_key` while it was
    /// online, creating the device entry if needed.
    pub fn set_device_identity(&mut self, device_key: &str, identity: DeviceIdentity) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .identity = Some(identity);
    }

    /// Whether `device_key` has a non-empty per-app binding overlay for the
    /// foreground app `app` (bundle id). Drives the menu-bar popover's "override
    /// active" badge — when the current app has its own bindings for this
    /// device, the global bindings are (partly) overridden.
    #[must_use]
    pub fn has_app_override(&self, device_key: &str, app: &str) -> bool {
        self.devices.get(device_key).is_some_and(|d| {
            d.per_app_bindings
                .get(app)
                .is_some_and(|overlay| !overlay.is_empty())
        })
    }

    /// Iterate every device we've recorded an identity for, as
    /// `(config_key, identity)`. Used to seed offline placeholder cards so a
    /// known device stays visible (with its panels) before any live probe.
    pub fn known_identities(&self) -> impl Iterator<Item = (&str, &DeviceIdentity)> {
        self.devices
            .iter()
            .filter_map(|(k, d)| d.identity.as_ref().map(|i| (k.as_str(), i)))
    }

    /// The lighting config for `device_key`, or `None` if unset.
    #[must_use]
    pub fn lighting(&self, device_key: &str) -> Option<Lighting> {
        self.devices
            .get(device_key)
            .and_then(|d| d.lighting.clone())
    }

    /// Replace the lighting config for `device_key`.
    pub fn set_lighting(&mut self, device_key: &str, lighting: Lighting) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .lighting = Some(lighting);
    }

    /// The committed sensor DPI for `device_key`, or `None` if never set.
    #[must_use]
    pub fn dpi(&self, device_key: &str) -> Option<u32> {
        self.devices.get(device_key).and_then(|d| d.dpi)
    }

    /// Record the committed sensor DPI for `device_key`, so the agent can
    /// re-apply it when the device reconnects (#189).
    pub fn set_dpi(&mut self, device_key: &str, dpi: u32) {
        self.devices.entry(device_key.to_string()).or_default().dpi = Some(dpi);
    }

    /// The SmartShift wheel config for `device_key`, or `None` if never set.
    #[must_use]
    pub fn smartshift(&self, device_key: &str) -> Option<SmartShift> {
        self.devices.get(device_key).and_then(|d| d.smartshift)
    }

    /// Record the SmartShift wheel config for `device_key`, so the agent can
    /// re-apply it when the device reconnects (#189).
    pub fn set_smartshift(&mut self, device_key: &str, smartshift: SmartShift) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .smartshift = Some(smartshift);
    }

    /// Whether `device_key`'s scroll wheel is inverted (issue #126). `false`
    /// (the native direction) for an unconfigured or absent device.
    #[must_use]
    pub fn invert_scroll(&self, device_key: &str) -> bool {
        self.devices
            .get(device_key)
            .is_some_and(|d| d.invert_scroll)
    }

    /// Set whether `device_key`'s scroll wheel is inverted. The agent reads this
    /// on the next `ReloadConfig` and applies it in the OS hook.
    pub fn set_invert_scroll(&mut self, device_key: &str, invert: bool) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .invert_scroll = invert;
    }

    /// The configured wheel resolution for `device_key`, or `None` when
    /// OpenLogi should leave the device's current resolution unchanged.
    #[must_use]
    pub fn scroll_resolution(&self, device_key: &str) -> Option<ScrollResolution> {
        self.devices
            .get(device_key)
            .and_then(|device| device.scroll_resolution)
    }

    /// Set the wheel resolution OpenLogi should restore for `device_key`.
    /// Passing `None` returns the device to its unmanaged default state.
    pub fn set_scroll_resolution(
        &mut self,
        device_key: &str,
        resolution: Option<ScrollResolution>,
    ) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .scroll_resolution = resolution;
    }
}

/// Reject reserved experimental multi-gesture field names on v4+ device tables.
fn reject_experimental_gesture_layout(raw: &toml::Value, path: &Path) -> Result<(), ConfigError> {
    let Some(devices) = raw.get("devices").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (device_key, device) in devices {
        let Some(table) = device.as_table() else {
            continue;
        };
        for marker in EXPERIMENTAL_GESTURE_MARKERS {
            if table.contains_key(*marker) {
                return Err(ConfigError::UnsupportedExperimentalGesture {
                    path: path.to_path_buf(),
                    device: device_key.clone(),
                    marker: (*marker).to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Write `bytes` to `path` atomically via a randomized temp file + rename,
/// with the directory fsync the old hand-rolled writer lacked.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg_attr(
        not(unix),
        expect(unused_mut, reason = "only the unix path mutates the options")
    )]
    let mut options = AtomicWriteFile::options();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Force 0600 on every save, matching the previous writer.
        options.preserve_mode(false).mode(0o600);
    }
    let mut file = options.open(path)?;
    io::Write::write_all(&mut file, bytes)?;
    file.commit()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "expect/unwrap are idiomatic in tests")]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::binding::default_gesture_binding;

    fn write_and_read(config: &Config) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        config.save_to_path(&path).expect("save");
        Config::load_from_path(&path).expect("load")
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.toml");
        let cfg = Config::load_from_path(&path).expect("load");
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert!(cfg.devices.is_empty());
    }

    #[test]
    fn lighting_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_lighting(
            "g513",
            Lighting {
                enabled: true,
                color: "00aabb".parse().expect("valid hex"),
                brightness: 75,
            },
        );
        let restored = write_and_read(&cfg);
        assert_eq!(
            restored.lighting("g513"),
            Some(Lighting {
                enabled: true,
                color: "00aabb".parse().expect("valid hex"),
                brightness: 75,
            })
        );
        assert_eq!(restored.lighting("absent"), None);
    }

    #[test]
    fn unparseable_lighting_color_falls_back_to_white() {
        let cfg: Config = toml::from_str(
            r#"
                schema_version = 3
                [devices.g513.lighting]
                enabled = true
                color = "red"
                brightness = 50
            "#,
        )
        .expect("config with a bad color still loads");
        assert_eq!(
            cfg.lighting("g513").map(|l| l.color),
            Some(crate::color::Rgb::WHITE)
        );
    }

    #[test]
    fn hash_prefixed_lighting_color_migrates_to_canonical_hex() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r##"
                schema_version = 3
                [devices.g513.lighting]
                enabled = true
                color = "#ff0000"
                brightness = 50
            "##,
        )
        .expect("write config");

        let cfg = Config::load_from_path(&path).expect("load hash-prefixed color");
        assert_eq!(
            cfg.lighting("g513").map(|lighting| lighting.color),
            Some(crate::color::Rgb::new(0xff, 0x00, 0x00))
        );

        cfg.save_to_path(&path).expect("save canonical color");
        let saved = fs::read_to_string(path).expect("read saved config");
        assert!(saved.contains("color = \"ff0000\""));
        assert!(!saved.contains("color = \"#"));
    }

    #[test]
    fn dpi_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_dpi("2b042", 1600);
        let restored = write_and_read(&cfg);
        assert_eq!(restored.dpi("2b042"), Some(1600));
        assert_eq!(restored.dpi("absent"), None);
    }

    #[test]
    fn smartshift_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_smartshift(
            "2b042",
            SmartShift {
                mode: WheelMode::Ratchet,
                auto_disengage: 16,
                tunable_torque: 30,
            },
        );
        let restored = write_and_read(&cfg);
        assert_eq!(
            restored.smartshift("2b042"),
            Some(SmartShift {
                mode: WheelMode::Ratchet,
                auto_disengage: 16,
                tunable_torque: 30,
            })
        );
        assert_eq!(restored.smartshift("absent"), None);
    }

    #[test]
    fn invert_scroll_roundtrips_per_device() {
        let mut cfg = Config::default();
        // Default is the native direction for any device, present or not.
        assert!(!cfg.invert_scroll("2b042"));
        cfg.set_invert_scroll("2b042", true);
        let restored = write_and_read(&cfg);
        assert!(restored.invert_scroll("2b042"));
        assert!(!restored.invert_scroll("absent"));
    }

    #[test]
    fn default_invert_scroll_is_omitted_from_toml() {
        // A device block with only the default (false) invert_scroll must not
        // emit the field — `skip_serializing_if` keeps configs clean.
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_invert_scroll("2b042", false);
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("invert_scroll"),
            "default invert_scroll should be omitted: {body}"
        );
    }

    #[test]
    fn scroll_resolution_roundtrips_all_three_states() {
        let mut cfg = Config::default();
        assert_eq!(cfg.scroll_resolution("mouse"), None);

        cfg.set_scroll_resolution("mouse", Some(ScrollResolution::Low));
        let low = write_and_read(&cfg);
        assert_eq!(low.scroll_resolution("mouse"), Some(ScrollResolution::Low));

        cfg.set_scroll_resolution("mouse", Some(ScrollResolution::High));
        let high = write_and_read(&cfg);
        assert_eq!(
            high.scroll_resolution("mouse"),
            Some(ScrollResolution::High)
        );

        cfg.set_scroll_resolution("mouse", None);
        let unmanaged = write_and_read(&cfg);
        assert_eq!(unmanaged.scroll_resolution("mouse"), None);
    }

    #[test]
    fn unset_scroll_resolution_is_omitted_from_toml() {
        let mut cfg = Config::default();
        cfg.set_binding("mouse", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_scroll_resolution("mouse", Some(ScrollResolution::Low));
        cfg.set_scroll_resolution("mouse", None);

        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("scroll_resolution"),
            "unset scroll resolution should be omitted: {body}"
        );
    }

    #[test]
    fn config_without_scroll_resolution_loads_as_unmanaged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r"
                schema_version = 3
                [devices.mouse]
                invert_scroll = true
            ",
        )
        .expect("write config");

        let cfg = Config::load_from_path(&path).expect("load existing config");
        assert_eq!(cfg.scroll_resolution("mouse"), None);
        assert!(cfg.invert_scroll("mouse"));
    }

    #[test]
    fn bindings_roundtrip_per_device() {
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_binding(
            "2b042",
            ButtonId::DpiToggle,
            Binding::Single(Action::CustomShortcut(crate::binding::KeyCombo {
                modifiers: crate::binding::KeyCombo::MOD_CMD,
                key_code: 0x23, // kVK_ANSI_P
                display: "⌘P".into(),
            })),
        );
        cfg.set_binding("4082d", ButtonId::Back, Binding::Single(Action::Paste));

        let parsed = write_and_read(&cfg);

        // Per-device isolation.
        let a = parsed.bindings_for("2b042");
        assert_eq!(a.get(&ButtonId::Back), Some(&Binding::Single(Action::Copy)));
        assert_eq!(
            a.get(&ButtonId::DpiToggle),
            Some(&Binding::Single(Action::CustomShortcut(
                crate::binding::KeyCombo {
                    modifiers: crate::binding::KeyCombo::MOD_CMD,
                    key_code: 0x23,
                    display: "⌘P".into(),
                }
            )))
        );

        let b = parsed.bindings_for("4082d");
        assert_eq!(
            b.get(&ButtonId::Back),
            Some(&Binding::Single(Action::Paste))
        );
        // Schema-v4 load Infer stamps the default `{GestureButton}` live set and
        // materializes its main default map — so a device that only stored Back
        // also gains the gesture-button entry. Per-device isolation still holds:
        // device b must not see device a's Back/DpiToggle bindings.
        assert!(
            b.contains_key(&ButtonId::GestureButton),
            "default gesture owner materializes on load"
        );
        assert!(!b.contains_key(&ButtonId::DpiToggle));
        assert_eq!(
            a.get(&ButtonId::Back),
            Some(&Binding::Single(Action::Copy)),
            "device a keeps its own Back binding"
        );

        // Unknown device returns empty map without panic.
        assert!(parsed.bindings_for("deadbeef").is_empty());
    }

    #[test]
    fn human_readable_toml_layout() {
        let mut cfg = Config::default();
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        let body = toml::to_string_pretty(&cfg).expect("serialize");

        // The key only contains [A-Za-z0-9_], so TOML emits it as a bare-word
        // table key (no surrounding quotes). The test asserts the observable
        // structure rather than locking in a specific quoting.
        assert!(body.contains("schema_version = 4"), "got: {body}");
        assert!(body.contains("[devices.2b042.bindings]"), "got: {body}");
        // A `Single` binding serializes byte-identically to the pre-v2 bare
        // `Action`, so the leaf line is unchanged.
        assert!(body.contains("Back = \"BrowserBack\""), "got: {body}");
    }

    #[test]
    fn dpi_presets_roundtrip_per_device() {
        let mut cfg = Config::default();
        cfg.set_dpi_presets("2b042", vec![800, 1600, 3200]);
        cfg.set_dpi_presets("4082d", vec![400, 1600]);

        let parsed = write_and_read(&cfg);

        assert_eq!(parsed.dpi_presets("2b042"), vec![800, 1600, 3200]);
        assert_eq!(parsed.dpi_presets("4082d"), vec![400, 1600]);
        assert!(parsed.dpi_presets("unknown").is_empty());
    }

    #[test]
    fn empty_dpi_presets_skip_serialization() {
        let mut cfg = Config::default();
        // Add a binding so the device block exists.
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_dpi_presets("2b042", vec![800]);
        cfg.set_dpi_presets("2b042", vec![]); // clear

        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("dpi_presets"),
            "empty dpi_presets should be omitted: {body}"
        );
    }

    #[test]
    fn device_identity_roundtrips_and_is_iterable() {
        use crate::device::{Capabilities, DeviceKind};

        let mut cfg = Config::default();
        let mouse = DeviceIdentity {
            display_name: "MX Master 3S".to_string(),
            model_info: None,
            codename: None,
            kind: DeviceKind::Mouse,
            capabilities: Capabilities {
                buttons: true,
                pointer: true,
                lighting: false,
                scroll_inversion: false,
                hires_wheel: true,
            },
        };
        cfg.set_device_identity("2b034", mouse.clone());
        // Recording an identity must not disturb unrelated per-device state.
        cfg.set_binding(
            "2b034",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );

        let parsed = write_and_read(&cfg);
        assert_eq!(parsed.device_identity("2b034"), Some(&mouse));
        assert_eq!(parsed.device_identity("absent"), None);
        assert_eq!(
            parsed.bindings_for("2b034").get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack)),
            "identity must coexist with bindings on the same device block"
        );
        assert_eq!(
            parsed.known_identities().collect::<Vec<_>>(),
            vec![("2b034", &mouse)]
        );
    }

    #[test]
    fn selected_device_roundtrips() {
        let mut cfg = Config::default();
        assert_eq!(cfg.selected_device(), None);
        cfg.set_selected_device(Some("2b042".into()));
        let parsed = write_and_read(&cfg);
        assert_eq!(parsed.selected_device(), Some("2b042"));
    }

    #[test]
    fn per_app_overlay_takes_precedence() {
        let mut cfg = Config::default();
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        cfg.set_binding(
            "2b042",
            ButtonId::Forward,
            Binding::Single(Action::BrowserForward),
        );
        cfg.set_per_app_binding(
            "2b042",
            "com.microsoft.VSCode",
            ButtonId::Back,
            Some(Action::Undo),
        );

        // Global: both buttons are browser nav.
        let global = cfg.effective_bindings("2b042", None);
        assert_eq!(
            global.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        assert_eq!(
            global.get(&ButtonId::Forward),
            Some(&Binding::Single(Action::BrowserForward))
        );

        // VSCode: Back overridden (wrapped as Single), Forward inherits.
        let vscode = cfg.effective_bindings("2b042", Some("com.microsoft.VSCode"));
        assert_eq!(
            vscode.get(&ButtonId::Back),
            Some(&Binding::Single(Action::Undo))
        );
        assert_eq!(
            vscode.get(&ButtonId::Forward),
            Some(&Binding::Single(Action::BrowserForward))
        );

        // Unrelated app falls through.
        let other = cfg.effective_bindings("2b042", Some("com.apple.Safari"));
        assert_eq!(
            other.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
    }

    #[test]
    fn per_app_binding_removal_prunes_empty_app() {
        let mut cfg = Config::default();
        cfg.set_per_app_binding(
            "2b042",
            "com.example.App",
            ButtonId::Back,
            Some(Action::Copy),
        );
        cfg.set_per_app_binding("2b042", "com.example.App", ButtonId::Back, None);
        assert!(
            cfg.devices["2b042"].per_app_bindings.is_empty(),
            "removing last override should prune the app entry"
        );
    }

    #[test]
    fn app_settings_default_omits_block() {
        let cfg = Config::default();
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("app_settings"),
            "default app_settings should be omitted: {body}"
        );
    }

    #[test]
    fn app_settings_launch_at_login_roundtrips() {
        let mut cfg = Config::default();
        cfg.app_settings.launch_at_login = true;
        let parsed = write_and_read(&cfg);
        assert!(parsed.app_settings.launch_at_login);
    }

    #[test]
    fn asset_source_preference_roundtrips() {
        let mut cfg = Config::default();
        cfg.app_settings.asset_source = AssetSourcePreference::OpenLogi;

        let body = toml::to_string_pretty(&cfg).expect("serialize");
        let parsed = write_and_read(&cfg);

        assert!(body.contains("asset_source = \"openlogi\""));
        assert_eq!(
            parsed.app_settings.asset_source,
            AssetSourcePreference::OpenLogi
        );
    }

    #[test]
    fn config_without_asset_source_keeps_automatic_selection() {
        let parsed: Config = toml::from_str(
            r"
                schema_version = 3
                [app_settings]
                auto_download_assets = false
            ",
        )
        .expect("config predating the asset-source setting loads");

        assert_eq!(
            parsed.app_settings.asset_source,
            AssetSourcePreference::Automatic
        );
    }

    #[test]
    fn cleared_selected_device_omits_field() {
        let mut cfg = Config::default();
        cfg.set_selected_device(Some("2b042".into()));
        cfg.set_selected_device(None);
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("selected_device"),
            "cleared selection should not appear: {body}"
        );
    }

    #[test]
    fn empty_device_block_is_skipped_in_output() {
        // Inserting then clearing should not leave a [devices."x"] header
        // with no bindings under it (skip_serializing_if on bindings).
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.devices
            .get_mut("2b042")
            .expect("entry")
            .bindings
            .clear();
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("Back"),
            "cleared bindings should not appear: {body}"
        );
    }

    #[test]
    fn migrates_v1_button_and_gesture_bindings() {
        // A pre-v2 file: split button_bindings + a flat gesture_bindings map.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
Back = \"BrowserBack\"

[devices.2b042.gesture_bindings]
Up = \"Copy\"
Click = \"Paste\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        // v1 still loads (version <= current) and folds into the merged map.
        let cfg = Config::load_from_path(&path).expect("load v1");
        let bindings = cfg.bindings_for("2b042");
        assert_eq!(
            bindings.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        let mut gesture = BTreeMap::new();
        gesture.insert(GestureDirection::Up, Action::Copy);
        gesture.insert(GestureDirection::Click, Action::Paste);
        assert_eq!(
            bindings.get(&ButtonId::GestureButton),
            Some(&Binding::Gesture(gesture))
        );

        // Saving self-heals to the current shape: stamped version + merged table,
        // legacy field names gone.
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(body.contains("schema_version = 4"), "got: {body}");
        assert!(body.contains("[devices.2b042.bindings]"), "got: {body}");
        assert!(!body.contains("button_bindings"), "got: {body}");
        assert!(!body.contains("gesture_bindings"), "got: {body}");
    }

    #[test]
    fn migration_gesture_map_wins_over_legacy_single_gesture_button_entry() {
        // The data-loss guard: when a legacy single button_bindings[GestureButton]
        // entry coexists with a gesture_bindings map (reachable via hand-edited
        // or very old configs), the gesture map must survive — not be shadowed by
        // the single entry. Mirrors the pre-v2 "gesture entries win" rule.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
GestureButton = \"MissionControl\"

[devices.2b042.gesture_bindings]
Up = \"Copy\"
Down = \"Paste\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        let cfg = Config::load_from_path(&path).expect("load v1");
        let mut gesture = BTreeMap::new();
        gesture.insert(GestureDirection::Up, Action::Copy);
        gesture.insert(GestureDirection::Down, Action::Paste);
        assert_eq!(
            cfg.bindings_for("2b042").get(&ButtonId::GestureButton),
            Some(&Binding::Gesture(gesture)),
            "gesture map must win over the legacy single GestureButton entry"
        );
    }

    #[test]
    fn migration_drops_vestigial_lone_gesture_button_single() {
        // A v1 file with only `button_bindings[GestureButton]` and no
        // `gesture_bindings` (the pre-gesture-picker shape). That entry never
        // dispatched in v1 — the gesture button's plain press routes through the
        // gesture `Click` slot, not the per-button map — so migrating it to a
        // `Binding::Single` would leave an unreachable entry the GUI hides and the
        // runtime ignores. It must be dropped, not shadow the gesture path.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
GestureButton = \"MissionControl\"
Back = \"BrowserBack\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        let bindings = Config::load_from_path(&path)
            .expect("load v1")
            .bindings_for("2b042");
        // An ordinary button still migrates to a `Single`...
        assert_eq!(
            bindings.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        // ...but the vestigial gesture-button Single is not kept as Single. Schema
        // v4 Infer stamps the default live owner and materializes the main
        // gesture pack (same actions as today's defaults), never an unreachable
        // Single(MissionControl) from button_bindings.
        match bindings.get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                for dir in GestureDirection::ALL {
                    assert_eq!(map.get(&dir), Some(&default_gesture_binding(dir)));
                }
            }
            other => panic!("expected materialized default Gesture map, got {other:?}"),
        }
    }

    #[test]
    fn rejects_newer_schema_version_but_accepts_v1() {
        // A future version is rejected loudly; the current and older versions
        // load (older ones migrate through the shim).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "schema_version = 99\n").expect("write");
        assert_matches!(
            Config::load_from_path(&path).expect_err("v99 should fail"),
            ConfigError::UnsupportedSchemaVersion { found: 99, .. }
        );

        fs::write(&path, "schema_version = 1\n").expect("write");
        assert!(
            Config::load_from_path(&path).is_ok(),
            "v1 should still load"
        );
    }

    #[test]
    fn set_gesture_direction_upgrades_single_to_gesture() {
        let mut cfg = Config::default();
        // Start from a Single binding, then bind a swipe direction.
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        cfg.set_gesture_direction("2b042", ButtonId::Back, GestureDirection::Up, Action::Copy);

        match cfg.bindings_for("2b042").get(&ButtonId::Back) {
            Some(Binding::Gesture(map)) => {
                // The prior single action is preserved as the Click entry.
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::BrowserBack)
                );
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
            }
            other => panic!("expected Gesture after upgrade, got {other:?}"),
        }
    }

    #[test]
    fn set_gesture_direction_on_fresh_gesture_button_seeds_click() {
        // Binding one direction on a never-configured gesture button must still
        // persist a `Click`, so the click projection is the canonical default
        // rather than `Action::None` (which reads as a no-op press).
        let mut cfg = Config::default();
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::Copy,
        );

        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&crate::binding::default_gesture_binding(
                        GestureDirection::Click
                    )),
                    "a fresh gesture button must seed a Click from its default"
                );
            }
            other => panic!("expected Gesture, got {other:?}"),
        }
    }

    #[test]
    fn gesture_owner_defaults_to_hidpp_button_yields_to_oshook_and_can_be_off() {
        let mut cfg = Config::default();
        // Default: the dedicated HID++ gesture button owns the gesture role even with no config.
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));

        // A dedicated HID++ gesture binding keeps it the owner.
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::MissionControl,
        );
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));

        // An explicit OS-hook gesture button takes the role over.
        cfg.set_binding(
            "2b042",
            ButtonId::Forward,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Copy)])),
        );
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::Forward));

        // Turning gestures off explicitly yields `None` (not the HID++ button default).
        let mut off = Config::default();
        off.disable_gestures("2b042");
        assert_eq!(off.gesture_owner("2b042"), None);
    }

    #[test]
    fn set_gesture_owner_records_owner_without_destroying_other_maps() {
        let mut cfg = Config::default();
        // Customize the dedicated HID++ gesture button's Up swipe; it is the (inferred) owner.
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::Copy,
        );
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));

        // Promote Back: the sole-owner shim becomes Back; the HID++ gesture button keeps
        // its full gesture map (no destructive demotion).
        cfg.set_binding("2b042", ButtonId::Back, Action::BrowserBack.into());
        cfg.set_gesture_owner("2b042", ButtonId::Back);
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::Back));

        let bindings = cfg.bindings_for("2b042");
        // Back: prior Single → Click, other dirs Action::None (K5b).
        match bindings.get(&ButtonId::Back) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::BrowserBack)
                );
                assert_eq!(
                    map.get(&GestureDirection::Up),
                    Some(&Action::None),
                    "promoted Single keeps Click only; swipes are None until user edits"
                );
            }
            other => panic!("expected Back to be a gesture binding, got {other:?}"),
        }
        // The HID++ gesture button's customized map survived the switch intact.
        match bindings.get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
            }
            other => panic!("expected the HID++ gesture button map preserved, got {other:?}"),
        }

        // Switching back restores the user's customization, not defaults
        // (regression guard: owner-switch used to discard the swipe arms).
        cfg.set_gesture_owner("2b042", ButtonId::GestureButton);
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));
        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
            }
            other => panic!("expected preserved gesture map, got {other:?}"),
        }
    }

    #[test]
    fn set_gesture_owner_seeds_a_fresh_button_with_full_directions() {
        let mut cfg = Config::default();
        // The dedicated HID++ gesture button gets the main default five-pack (K5b).
        cfg.set_gesture_owner("2b042", ButtonId::GestureButton);
        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                for dir in GestureDirection::ALL {
                    assert_eq!(map.get(&dir), Some(&default_gesture_binding(dir)));
                }
            }
            other => panic!("expected full default gesture map, got {other:?}"),
        }

        // A vacant OS-hook button gets all five dirs as Action::None (K5b) — not
        // the desktop defaults (those are Gesture Button / Window navigation only).
        cfg.set_gesture_owner("2b042", ButtonId::Forward);
        match cfg.bindings_for("2b042").get(&ButtonId::Forward) {
            Some(Binding::Gesture(map)) => {
                for dir in GestureDirection::ALL {
                    assert_eq!(
                        map.get(&dir),
                        Some(&Action::None),
                        "vacant non-GB first enable is all None"
                    );
                }
            }
            other => panic!("expected full None gesture map for Forward, got {other:?}"),
        }
    }

    #[test]
    fn disable_gestures_turns_off_without_destroying_maps() {
        let mut cfg = Config::default();
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::Copy,
        );
        cfg.disable_gestures("2b042");
        // Off, but the HID++ gesture button's customized map is preserved (re-enabling
        // restores it rather than resurrecting a wiped default).
        assert_eq!(cfg.gesture_owner("2b042"), None);
        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
            }
            other => panic!("expected the gesture map preserved while off, got {other:?}"),
        }
    }

    #[test]
    fn gesture_buttons_roundtrip_as_off_or_array() {
        let mut cfg = Config::default();
        cfg.set_gesture_owner("2b042", ButtonId::Back); // singleton
        cfg.disable_gestures("4082d"); // explicit off
        cfg.set_gesture_buttons(
            "mx",
            [ButtonId::GestureButton, ButtonId::DpiToggle],
        );

        let parsed = write_and_read(&cfg);
        assert_eq!(parsed.gesture_owner("2b042"), Some(ButtonId::Back));
        assert_eq!(parsed.gesture_owner("4082d"), None);
        assert!(
            parsed
                .gesture_buttons("mx")
                .contains(ButtonId::GestureButton)
        );
        assert!(parsed.gesture_buttons("mx").contains(ButtonId::DpiToggle));

        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            body.contains("gesture_buttons") && body.contains("Back"),
            "got: {body}"
        );
        assert!(body.contains("gesture_buttons = \"Off\""), "got: {body}");
        assert!(
            !body.contains("gesture_owner"),
            "legacy sole-owner field must not serialize: {body}"
        );
    }

    #[test]
    fn invalid_gesture_owner_string_is_tolerated_not_fatal() {
        // A hand-edit typo in gesture_owner must NOT fail the whole-document parse
        // (which would revert every device's settings to defaults). It degrades
        // to "infer" while the rest of the device config survives.
        let toml = "\
schema_version = 2

[devices.2b042]
gesture_owner = \"bogus\"

[devices.2b042.bindings]
Back = \"Copy\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, toml).expect("write");

        let cfg =
            Config::load_from_path(&path).expect("an invalid gesture_owner must not fail the load");
        // The rest of the device config survived...
        assert_eq!(
            cfg.bindings_for("2b042").get(&ButtonId::Back),
            Some(&Binding::Single(Action::Copy))
        );
        // ...and the bad owner degraded to inference (HID++ button default here).
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));
        assert!(
            cfg.gesture_buttons("2b042")
                .contains(ButtonId::GestureButton)
        );
    }

    #[test]
    fn migration_matrix_v3_owner_and_infer_paths() {
        let load = |body: &str| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("config.toml");
            fs::write(&path, body).expect("write");
            Config::load_from_path(&path).expect("load")
        };

        // v3 owner=GB, no Dpi Gesture → {GestureButton}
        let cfg = load(
            r#"
schema_version = 3
[devices.d]
gesture_owner = "GestureButton"
"#,
        );
        assert!(
            cfg.gesture_buttons("d").contains(ButtonId::GestureButton)
                && cfg.gesture_buttons("d").len() == 1
        );

        // v3 owner=GB, Dpi=Gesture → {GB, DpiToggle}
        let cfg = load(
            r#"
schema_version = 3
[devices.d]
gesture_owner = "GestureButton"
[devices.d.bindings.DpiToggle]
Up = "VolumeUp"
"#,
        );
        let live = cfg.gesture_buttons("d");
        assert!(live.contains(ButtonId::GestureButton));
        assert!(live.contains(ButtonId::DpiToggle));

        // v3 owner=Off → Off
        let cfg = load(
            r#"
schema_version = 3
[devices.d]
gesture_owner = "Off"
"#,
        );
        assert!(cfg.gesture_buttons("d").is_off());

        // v3 no owner, Forward Gesture → {Forward}
        let cfg = load(
            r#"
schema_version = 3
[devices.d.bindings.Forward]
Up = "Copy"
"#,
        );
        let live = cfg.gesture_buttons("d");
        assert!(live.contains(ButtonId::Forward));
        assert!(!live.contains(ButtonId::GestureButton));

        // v3 no owner, only Dpi Gesture → {DpiToggle} (not dual with GB)
        let cfg = load(
            r#"
schema_version = 3
[devices.d.bindings.DpiToggle]
Up = "VolumeUp"
"#,
        );
        let live = cfg.gesture_buttons("d");
        assert!(live.contains(ButtonId::DpiToggle));
        assert!(!live.contains(ButtonId::GestureButton));

        // v3 no owner, Back+Forward Gesture → {Back, Forward}
        let cfg = load(
            r#"
schema_version = 3
[devices.d.bindings.Back]
Up = "Copy"
[devices.d.bindings.Forward]
Up = "Paste"
"#,
        );
        let live = cfg.gesture_buttons("d");
        assert!(live.contains(ButtonId::Back));
        assert!(live.contains(ButtonId::Forward));
    }

    #[test]
    fn migration_v4_explicit_sticky_and_experimental_reject() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Explicit ["GestureButton"] + Dpi map → sticky, no promote
        let path = dir.path().join("sticky.toml");
        fs::write(
            &path,
            r#"
schema_version = 4
[devices.d]
gesture_buttons = ["GestureButton"]
[devices.d.bindings.GestureButton]
Up = "MissionControl"
Down = "ShowDesktop"
Left = "PrevTab"
Right = "NextTab"
Click = "AppExpose"
[devices.d.bindings.DpiToggle]
Up = "VolumeUp"
"#,
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load sticky");
        assert!(cfg.gesture_buttons("d").contains(ButtonId::GestureButton));
        assert!(!cfg.gesture_buttons("d").contains(ButtonId::DpiToggle));

        // Experimental marker → reject; file untouched
        let path = dir.path().join("experimental.toml");
        let body = r#"
schema_version = 4
[devices.d]
gesture_owners = ["GestureButton", "DpiToggle"]
"#;
        fs::write(&path, body).expect("write");
        let err = Config::load_from_path(&path).expect_err("experimental must reject");
        assert_matches!(err, ConfigError::UnsupportedExperimentalGesture { .. });
        assert_eq!(fs::read_to_string(&path).expect("reread"), body);

        // Empty array → Off
        let path = dir.path().join("empty.toml");
        fs::write(
            &path,
            r"
schema_version = 4
[devices.d]
gesture_buttons = []
",
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load empty");
        assert!(cfg.gesture_buttons("d").is_off());

        // Invalid scalar → parse error
        let path = dir.path().join("bad.toml");
        fs::write(
            &path,
            r#"
schema_version = 4
[devices.d]
gesture_buttons = "not-a-button"
"#,
        )
        .expect("write");
        let err = Config::load_from_path(&path).expect_err("invalid scalar");
        assert_matches!(err, ConfigError::Parse { .. });
    }

    #[test]
    fn enable_dual_hidpp_and_set_gesture_owner_noop_preserves_dual() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.enable_gesture_button("2b042", ButtonId::DpiToggle);
        let live = cfg.gesture_buttons("2b042");
        assert!(live.contains(ButtonId::GestureButton));
        assert!(live.contains(ButtonId::DpiToggle));

        // Compat sole-owner is GestureButton; re-set must not collapse dual (K14).
        assert_eq!(cfg.gesture_owner("2b042"), Some(ButtonId::GestureButton));
        cfg.set_gesture_owner("2b042", ButtonId::GestureButton);
        let live = cfg.gesture_buttons("2b042");
        assert!(live.contains(ButtonId::GestureButton));
        assert!(live.contains(ButtonId::DpiToggle));

        // Resolver: both LiveMap
        assert_matches!(
            cfg.resolve_gesture_button("2b042", ButtonId::GestureButton),
            GestureButtonState::LiveMap { .. }
        );
        assert_matches!(
            cfg.resolve_gesture_button("2b042", ButtonId::DpiToggle),
            GestureButtonState::LiveMap { .. }
        );
    }

    #[test]
    fn disable_gesture_button_preserves_maps() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("2b042", ButtonId::GestureButton);
        cfg.enable_gesture_button("2b042", ButtonId::DpiToggle);
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::DpiToggle,
            GestureDirection::Up,
            Action::VolumeUp,
        );
        cfg.disable_gesture_button("2b042", ButtonId::DpiToggle);
        assert!(!cfg.gesture_buttons("2b042").contains(ButtonId::DpiToggle));
        match cfg.resolve_gesture_button("2b042", ButtonId::DpiToggle) {
            GestureButtonState::NotLive {
                preserved: Some(map),
            } => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::VolumeUp));
            }
            other => panic!("expected preserved NotLive map, got {other:?}"),
        }
    }

    #[test]
    fn main_default_map_is_custom_not_window_navigation() {
        // K5a fact: main pack ≠ Window navigation 1:1.
        assert_ne!(
            main_default_gesture_map(),
            window_navigation_map(),
            "main defaults must not equal Window navigation"
        );
        assert_eq!(
            match_gesture_preset(&main_default_gesture_map()),
            GesturePreset::Custom
        );
        assert_eq!(
            match_gesture_preset(&window_navigation_map()),
            GesturePreset::WindowNavigation
        );
        assert_eq!(
            match_gesture_preset(&media_controls_map()),
            GesturePreset::MediaControls
        );
    }

    #[test]
    fn migration_main_map_stays_custom_actions_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // v3 owner=GB with main-pack directions (what OpenLogi ships today).
        fs::write(
            &path,
            r#"
schema_version = 3
[devices.d]
gesture_owner = "GestureButton"
[devices.d.bindings.GestureButton]
Up = "MissionControl"
Down = "ShowDesktop"
Left = "PrevTab"
Right = "NextTab"
Click = "AppExpose"
"#,
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load");
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map, &main_default_gesture_map());
            }
            other => panic!("expected main map preserved, got {other:?}"),
        }
    }

    #[test]
    fn apply_named_preset_replaces_map_edit_forces_custom() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("d", ButtonId::GestureButton);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::Custom
        );

        cfg.set_gesture_preset("d", ButtonId::GestureButton, GesturePreset::MediaControls);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::MediaControls
        );
        match cfg.bindings_for("d").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => assert_eq!(map, &media_controls_map()),
            other => panic!("expected media table, got {other:?}"),
        }

        cfg.set_gesture_direction(
            "d",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::Copy,
        );
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::PlayPause),
                    "other dirs keep the media pack"
                );
            }
            other => panic!("expected edited map, got {other:?}"),
        }
    }

    #[test]
    fn select_custom_does_not_wipe_map() {
        let mut cfg = Config::default();
        cfg.set_gesture_preset("d", ButtonId::DpiToggle, GesturePreset::MediaControls);
        cfg.set_gesture_preset("d", ButtonId::DpiToggle, GesturePreset::Custom);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::DpiToggle) {
            Some(Binding::Gesture(map)) => assert_eq!(map, &media_controls_map()),
            other => panic!("Custom must not wipe map, got {other:?}"),
        }
    }

    #[test]
    fn vacant_dpi_enable_is_custom_all_none() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("d", ButtonId::DpiToggle);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::DpiToggle) {
            Some(Binding::Gesture(map)) => {
                for dir in GestureDirection::ALL {
                    assert_eq!(map.get(&dir), Some(&Action::None));
                }
            }
            other => panic!("expected all-None map, got {other:?}"),
        }
    }

    #[test]
    fn vacant_gb_enable_is_custom_main_pack() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("d", ButtonId::GestureButton);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => assert_eq!(map, &main_default_gesture_map()),
            other => panic!("expected main pack, got {other:?}"),
        }
    }

    #[test]
    fn exact_media_map_on_load_tags_media() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
schema_version = 4
[devices.d]
gesture_buttons = ["DpiToggle"]
[devices.d.bindings.DpiToggle]
Left = "PrevTrack"
Right = "NextTrack"
Up = "VolumeUp"
Down = "VolumeDown"
Click = "PlayPause"
"#,
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load");
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::MediaControls
        );
    }

    #[test]
    fn apply_window_navigation_writes_desktop_pack() {
        let mut cfg = Config::default();
        cfg.set_gesture_preset(
            "d",
            ButtonId::GestureButton,
            GesturePreset::WindowNavigation,
        );
        match cfg.bindings_for("d").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => assert_eq!(map, &window_navigation_map()),
            other => panic!("expected window nav table, got {other:?}"),
        }
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::GestureButton),
            GesturePreset::WindowNavigation
        );
    }

    #[test]
    fn re_enable_after_single_rebind_resets_stale_preset_tag() {
        let mut cfg = Config::default();
        cfg.enable_gesture_button("d", ButtonId::DpiToggle);
        cfg.set_gesture_preset("d", ButtonId::DpiToggle, GesturePreset::MediaControls);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::MediaControls
        );

        // Single rebind destroys map + preset and drops from live set (K13).
        cfg.set_binding(
            "d",
            ButtonId::DpiToggle,
            Binding::Single(Action::CycleDpiPresets),
        );
        assert!(!cfg.gesture_buttons("d").contains(ButtonId::DpiToggle));
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::Custom
        );

        // Re-promote → Custom click-only map, not a stale MediaControls tag.
        cfg.enable_gesture_button("d", ButtonId::DpiToggle);
        assert_eq!(
            cfg.gesture_preset("d", ButtonId::DpiToggle),
            GesturePreset::Custom
        );
        match cfg.bindings_for("d").get(&ButtonId::DpiToggle) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::CycleDpiPresets)
                );
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::None));
            }
            other => panic!("expected upgraded gesture map, got {other:?}"),
        }
    }

    #[test]
    fn identity_only_v3_does_not_invent_dpi_gesture() {
        // Realistic existing-user shape: schema 3, device identity only, no
        // gesture_owner and no bindings. Migration must not invent a DPI
        // gesture map or put DpiToggle in the live set (K5a / K6a).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
schema_version = 3
[devices."receiver:c0ea968a:slot:1".identity]
display_name = "MX Master 3"
kind = "mouse"
[devices."receiver:c0ea968a:slot:1".identity.capabilities]
buttons = true
pointer = true
lighting = false
scroll_inversion = true
hires_wheel = true
"#,
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load");
        let key = "receiver:c0ea968a:slot:1";
        let live = cfg.gesture_buttons(key);
        assert!(
            live.contains(ButtonId::GestureButton) && live.len() == 1,
            "default owner only: {live}"
        );
        assert!(
            !live.contains(ButtonId::DpiToggle),
            "must not invent DPI live: {live}"
        );
        assert!(
            !matches!(
                cfg.bindings_for(key).get(&ButtonId::DpiToggle),
                Some(Binding::Gesture(_))
            ),
            "must not invent a DPI gesture map"
        );
    }

    #[test]
    fn fold_owner_gb_with_dpi_single_does_not_promote_dpi() {
        // K6a complement: dual only when DpiToggle is already Gesture.
        // FoldOwner=GB + DPI as Single → live keeps GB (and OS-hook maps if
        // any); DpiToggle must not enter the live set and its Single binding
        // must not be rewritten to Gesture.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
schema_version = 3
[devices.mx]
gesture_owner = "GestureButton"
[devices.mx.bindings]
DpiToggle = "CycleDpiPresets"
[devices.mx.bindings.MiddleClick]
Up = "MissionControl"
Click = "MiddleClick"
[devices.mx.bindings.GestureButton]
Up = "MissionControl"
Down = "ShowDesktop"
Left = "PrevTab"
Right = "NextTab"
Click = "AppExpose"
"#,
        )
        .expect("write");
        let cfg = Config::load_from_path(&path).expect("load");
        let live = cfg.gesture_buttons("mx");
        assert!(live.contains(ButtonId::GestureButton), "{live}");
        assert!(
            live.contains(ButtonId::MiddleClick),
            "OS-hook multi-union keeps Middle: {live}"
        );
        assert!(
            !live.contains(ButtonId::DpiToggle),
            "DPI Single must not become live gesture: {live}"
        );
        assert_eq!(
            cfg.bindings_for("mx").get(&ButtonId::DpiToggle),
            Some(&Binding::Single(Action::CycleDpiPresets)),
            "DPI Single binding must be left unchanged"
        );
    }
}
