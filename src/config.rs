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

use std::{path::PathBuf, time::Duration};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub preview: Preview,
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

impl Config {
    /// Load the first config file there is, or the defaults.
    ///
    /// A malformed file is reported and then ignored rather than being fatal. Unlike a
    /// compositor, this program is bus-activated: refusing to start would make every screen
    /// share fail with nothing on screen to explain why, and the only thing in the file is how
    /// fast thumbnails refresh.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
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

    fn path() -> Option<PathBuf> {
        let user = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|dir| dir.join("wlrix").join("portal.toml"));

        user.filter(|path| path.exists())
            .or_else(|| Some(PathBuf::from("/etc/wlrix/portal.toml")).filter(|p| p.exists()))
    }
}
