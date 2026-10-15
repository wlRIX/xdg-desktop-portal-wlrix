// SPDX-License-Identifier: GPL-3.0-or-later
//! `portal.toml`, and the very little of this that is worth configuring.
//!
//! Read from `$XDG_CONFIG_HOME/wlrix/portal.toml` (or `~/.config/wlrix/portal.toml`), falling
//! back to `/etc/wlrix/portal.toml`. The first file found is used whole rather than merged, so
//! your own file is all of what you get -- the same rule `wlrix-idle` follows.
//!
//! Unknown keys are an error, also as elsewhere in wlRIX: a silently ignored typo in a config
//! file is a bad afternoon. There is no file by default and the defaults below are the ones
//! everything was tuned with.
//!
//! `[appearance]` is the exception to "very little worth configuring": it is not tuning, it is
//! the session telling this backend which color scheme is in force so it can pass that on to
//! toolkits. It is written by `wlrix-settings-daemon` rather than by hand -- see
//! [`crate::dbus::settings`].

use std::{path::PathBuf, time::Duration};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub preview: Preview,
    pub capture: Capture,
    pub appearance: Appearance,
    /// Not from `portal.toml`: the desktop's sound settings, read from `sounds.toml` beside it.
    /// See [`Sounds`].
    #[serde(skip)]
    pub sounds: Sounds,
    /// Not from `portal.toml` either: the pointer theme, read from `compositor.toml`'s
    /// `[cursor]`. See [`Cursor`].
    #[serde(skip)]
    pub cursor: Cursor,
}

/// What this backend reports through `org.freedesktop.impl.portal.Settings`.
///
/// Its own section rather than a bare key, matching `compositor.toml` and `desktop.toml` -- a
/// scheme is not the only thing that will ever go here, and the files read alike.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Appearance {
    /// A scheme id from `wlrix-ui`: `classic`, `classic-g10`, `classic-g24`, `gotham`.
    ///
    /// The same key the compositor, the desktop, the tray and the screenshot tool each carry;
    /// `wlrix-settings-daemon` writes all five together from `appearance.palette`, so a
    /// GTK application is told the same scheme the chrome around it is drawn in.
    ///
    /// Absent, empty or unrecognized means the default, which is what
    /// [`wlrix_ui::palette::resolve`] answers. A mistyped scheme name must not stop a toolkit
    /// being told anything at all.
    pub palette: Option<String>,
    /// The icon theme a toolkit is told to use, as `org.gnome.desktop.interface` `icon-theme`.
    ///
    /// Absent means [`DEFAULT_ICON_THEME`], the IRIX set `wlrix-assets` installs, so a GTK
    /// application draws the same folders and file types as wlRIX Files. Empty means this
    /// backend does not answer for it, and the next backend's answer -- GNOME's gsettings,
    /// through `xdg-desktop-portal-gtk` -- is what a toolkit gets.
    pub icon_theme: Option<String>,
}

/// What [`Appearance::icon_theme`] means when the file does not say.
pub const DEFAULT_ICON_THEME: &str = "wlrix";

impl Appearance {
    /// The icon theme to report, or `None` to leave it to the next backend.
    pub fn icon_theme(&self) -> Option<&str> {
        match self.icon_theme.as_deref() {
            None => Some(DEFAULT_ICON_THEME),
            Some("") => None,
            Some(theme) => Some(theme),
        }
    }
}

/// The desktop's sound theme and whether event sounds play, for
/// `org.gnome.desktop.sound` `theme-name` and `event-sounds`.
///
/// Read from `sounds.toml`, which belongs to `wlrix-sound` -- the settings are the sound
/// daemon's, and a second copy of them in `portal.toml` would be two places to change one thing.
/// This backend only reads the file, so it parses it leniently: `wlrix-sound` is the one that
/// validates it, and a key this backend does not know about is not this backend's business.
/// `wlrix-settings-daemon` sends `SIGHUP` here as well as to `wlrix-sound` when either key moves,
/// so running applications are told.
#[derive(Debug, Clone, PartialEq)]
pub struct Sounds {
    pub enabled: bool,
    pub theme: String,
}

/// What `theme` means when `sounds.toml` does not say: the IRIX set `wlrix-sounds` installs.
/// The same default `wlrix-sound` has, kept in step by hand.
pub const DEFAULT_SOUND_THEME: &str = "wlrix";

impl Default for Sounds {
    fn default() -> Self {
        Self {
            enabled: true,
            theme: DEFAULT_SOUND_THEME.to_owned(),
        }
    }
}

/// `sounds.toml` as this backend reads it. No `deny_unknown_fields`, on purpose -- see
/// [`Sounds`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundsFile {
    enabled: Option<bool>,
    theme: Option<String>,
}

impl Sounds {
    /// Read `sounds.toml` -- the user's, else the system's -- or the defaults.
    pub fn load() -> Self {
        let Some(path) = config_path("sounds.toml") else {
            return Self::default();
        };
        match std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| toml::from_str::<SoundsFile>(&text).map_err(|err| err.to_string()))
        {
            Ok(file) => Self::from_file(file),
            Err(err) => {
                tracing::warn!("could not read {}: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }

    fn from_file(file: SoundsFile) -> Self {
        let default = Self::default();
        Self {
            enabled: file.enabled.unwrap_or(default.enabled),
            // Empty means the default theme, as it does to `wlrix-sound`.
            theme: file
                .theme
                .filter(|theme| !theme.is_empty())
                .unwrap_or(default.theme),
        }
    }
}

/// The pointer theme and size, for `org.gnome.desktop.interface` `cursor-theme` and
/// `cursor-size`.
///
/// Read from `compositor.toml`'s `[cursor]`, because the compositor is what decides it: it draws
/// the pointer over the desktop and its own frames, and tells the session and XWayland which
/// theme that is. A copy in `portal.toml` would be a second place to change one thing, as with
/// [`Sounds`]. Parsed leniently for the same reason too -- the compositor validates its file, and
/// every other section in it is not this backend's business.
///
/// The fallbacks are the compositor's, in its order: the file, then `XCURSOR_THEME` and
/// `XCURSOR_SIZE`, then nothing for the theme and 24 for the size. "Nothing" rather than the
/// compositor's `default`: that is the name of whatever this machine happens to inherit, and
/// telling a toolkit so says nothing it does not already assume.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cursor {
    /// `None` when neither the file nor the environment names one.
    pub theme: Option<String>,
    pub size: u32,
}

/// What `size` means when neither `compositor.toml` nor `XCURSOR_SIZE` says. The compositor's
/// own default, kept in step by hand.
pub const DEFAULT_CURSOR_SIZE: u32 = 24;

/// The parts of `compositor.toml` this backend reads. No `deny_unknown_fields` -- see [`Cursor`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CompositorFile {
    cursor: CursorSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CursorSection {
    theme: Option<String>,
    size: Option<u32>,
}

impl Cursor {
    /// Read `compositor.toml` -- the user's, else the system's -- and the environment.
    pub fn load() -> Self {
        let file = config_path("compositor.toml")
            .and_then(|path| {
                std::fs::read_to_string(&path)
                    .map_err(|err| err.to_string())
                    .and_then(|text| {
                        toml::from_str::<CompositorFile>(&text).map_err(|err| err.to_string())
                    })
                    .inspect_err(|err| {
                        tracing::warn!("could not read {}: {err}; using defaults", path.display())
                    })
                    .ok()
            })
            .unwrap_or_default();
        Self::resolve(
            file.cursor,
            std::env::var("XCURSOR_THEME").ok().as_deref(),
            std::env::var("XCURSOR_SIZE").ok().as_deref(),
        )
    }

    /// The precedence, with the environment passed in so it can be tested without mutating the
    /// process's own.
    ///
    /// Empty and zero count as absent, as they do to the compositor: a settings panel that clears
    /// a field writes `""`, and that means what leaving the key out means.
    fn resolve(file: CursorSection, env_theme: Option<&str>, env_size: Option<&str>) -> Self {
        let named = |name: &String| !name.trim().is_empty();
        let theme = file
            .theme
            .filter(named)
            .or_else(|| env_theme.map(str::to_owned).filter(named))
            .map(|theme| theme.trim().to_owned());
        let size = file
            .size
            .filter(|&size| size > 0)
            .or_else(|| {
                env_size
                    .and_then(|size| size.trim().parse().ok())
                    .filter(|&s| s > 0)
            })
            .unwrap_or(DEFAULT_CURSOR_SIZE);
        Self { theme, size }
    }
}

/// How frames are got out of the compositor.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Capture {
    /// Render captures straight into GPU memory instead of copying them through the CPU.
    ///
    /// Safe to switch on: the stream offers shared memory alongside, so a consumer that cannot
    /// import a dmabuf falls back rather than failing. Verified -- GStreamer declines the
    /// dmabuf and streams over shm without a hiccup.
    ///
    /// **Off by default anyway**, because nothing has yet *accepted* one. The path is proven as
    /// far as the compositor, which renders into a dmabuf happily, but no consumer available
    /// here negotiates it, so the last stretch is untested against a real application. Turning
    /// this on is how that gets tested.
    ///
    /// Note this does not decide whether frames are copied: they are not, either way. The shm
    /// path renders into the memfd the consumer reads, exactly as the dmabuf path does.
    pub dmabuf: bool,
}

/// How the picker's thumbnails are produced. See [`crate::preview`] for why these are the knobs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Preview {
    /// How often *one* source is captured, in milliseconds.
    ///
    /// Not the refresh rate of a tile: sources take turns, so with ten of them and the default
    /// tick each tile is refreshed about once a second. Lower this to make the grid livelier
    /// and the readback more expensive.
    #[serde(rename = "tick_ms", deserialize_with = "milliseconds")]
    pub tick: Duration,
    /// Thumbnail size in pixels, as `[width, height]`.
    ///
    /// The capture always arrives at the source's full size -- the protocol has no server-side
    /// downscale -- so this changes how much is scaled *to*, not how much is read back.
    pub tile: (u32, u32),
}

impl Default for Preview {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(100),
            // 16:9, and small enough that a grid of them is a few megabytes.
            tile: (320, 180),
        }
    }
}

fn milliseconds<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let ms = u64::deserialize(d)?;
    // Zero would busy-loop the capture; there is no sane reading of it.
    Ok(Duration::from_millis(ms.max(1)))
}

/// Parse a candidate config file, for `--check-config`.
///
/// This program's own serde types are the authority on what `portal.toml` may contain.
/// `wlrix-settings-daemon` writes a temporary file and runs this against it before renaming it
/// into place, so a settings app cannot produce a file this backend would refuse -- which
/// matters because `deny_unknown_fields` means one wrong key costs the *whole* file.
///
/// Deliberately not [`Config::load`]: that warns and carries on with defaults, which is right
/// for a bus-activated backend (refusing to start would make every screen share fail with
/// nothing on screen to explain why) and exactly wrong here, where the question *is* whether
/// the file is acceptable.
pub fn check(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    toml::from_str::<Config>(&text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

impl Config {
    /// Load the first config file there is, or the defaults.
    ///
    /// A malformed file is reported and then ignored rather than being fatal. Unlike a
    /// compositor, this program is bus-activated: refusing to start would make every screen
    /// share fail with nothing on screen to explain why, and the only thing in the file is how
    /// fast thumbnails refresh.
    pub fn load() -> Self {
        let mut config = Self::load_file();
        config.sounds = Sounds::load();
        config.cursor = Cursor::load();
        config
    }

    /// `portal.toml` alone.
    fn load_file() -> Self {
        let Some(path) = config_path("portal.toml") else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!("could not read {}: {err}", path.display());
                return Self::default();
            }
        };
        match toml::from_str(&text) {
            Ok(config) => {
                tracing::debug!("read {}", path.display());
                config
            }
            Err(err) => {
                tracing::warn!("{} is not valid: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }
}

/// The first of the user's and the system's `wlrix/<name>` that exists.
fn config_path(name: &str) -> Option<PathBuf> {
    let user = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("wlrix").join(name));

    user.filter(|path| path.exists())
        .or_else(|| Some(PathBuf::from("/etc/wlrix").join(name)).filter(|p| p.exists()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_toml_is_read_leniently() {
        // `wlrix-sound` owns this file and validates it; a key it has and this backend does not
        // know -- `disabled` -- must not cost a toolkit the theme.
        let file: SoundsFile =
            toml::from_str("enabled = false\ntheme = \"freedesktop\"\ndisabled = [\"bell\"]\n")
                .expect("parses");
        let sounds = Sounds::from_file(file);
        assert!(!sounds.enabled);
        assert_eq!(sounds.theme, "freedesktop");
    }

    #[test]
    fn an_empty_sound_theme_is_the_default() {
        let file: SoundsFile = toml::from_str("theme = \"\"\n").expect("parses");
        assert_eq!(Sounds::from_file(file), Sounds::default());
    }

    fn section(theme: Option<&str>, size: Option<u32>) -> CursorSection {
        CursorSection {
            theme: theme.map(ToOwned::to_owned),
            size,
        }
    }

    #[test]
    fn the_compositors_cursor_section_is_read_leniently() {
        // Every other section of `compositor.toml` is the compositor's to validate.
        let file: CompositorFile =
            toml::from_str("[keyboard]\nlayout = \"us\"\n[cursor]\ntheme = \"sgi\"\nsize = 32\n")
                .expect("parses");
        let cursor = Cursor::resolve(file.cursor, None, None);
        assert_eq!(cursor.theme.as_deref(), Some("sgi"));
        assert_eq!(cursor.size, 32);
    }

    #[test]
    fn the_cursor_file_comes_before_the_environment() {
        let cursor = Cursor::resolve(section(Some("sgi"), Some(32)), Some("Adwaita"), Some("24"));
        assert_eq!(cursor.theme.as_deref(), Some("sgi"));
        assert_eq!(cursor.size, 32);
    }

    #[test]
    fn an_absent_or_empty_cursor_falls_back_to_the_environment() {
        let cursor = Cursor::resolve(section(Some(" "), Some(0)), Some("sgi"), Some("48"));
        assert_eq!(cursor.theme.as_deref(), Some("sgi"));
        assert_eq!(cursor.size, 48);
    }

    #[test]
    fn with_nothing_named_there_is_no_cursor_theme() {
        let cursor = Cursor::resolve(section(None, None), Some(""), Some("huge"));
        assert_eq!(cursor.theme, None);
        assert_eq!(cursor.size, DEFAULT_CURSOR_SIZE);
    }
}
