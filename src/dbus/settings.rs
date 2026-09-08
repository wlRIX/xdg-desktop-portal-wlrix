// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.Settings`: telling a toolkit what the desktop looks like.
//!
//! The one thing a GTK application cannot be told any other way. Its window buttons come from
//! `gtk-decoration-layout`, and on Wayland **GTK reads that from here and nowhere else** --
//! measured: with `settings.ini` saying one thing and this backend another, GTK 3 reported this
//! backend's answer and GTK 4 did too. Without a Settings backend GTK 3 keeps its own compiled
//! default, `menu:close`, and ignores `settings.ini` for that key entirely.
//!
//! One asymmetry to know: **GTK 3 only asks a portal when `GTK_USE_PORTAL=1`**, which
//! `wlrix-session`'s `start-wlrix.sh` exports for exactly this reason. GTK 4 asks unconditionally.
//!
//! ## What is served, and what is deliberately not
//!
//! Only the keys wlRIX actually owns:
//!
//! - `org.gnome.desktop.wm.preferences` `button-layout` -- the menu button on the left, minimize
//!   and maximize on the right, and no close button, which is what
//!   `wlrix-compositor`'s own `FrameStyle` has. IRIX kept Close in the window menu, and a
//!   window framed by this compositor still has one: a right-click anywhere on the border opens
//!   it, and it carries Close with Alt+F4 beside it. So dropping the close button costs no way
//!   of closing a window.
//!
//!   The `menu` part of that layout is aspirational. GTK 3 only draws a window-menu button for
//!   an application that has an app menu, and GTK 4 draws nothing for it at all -- verified
//!   against a live widget tree. It is asked for anyway: it costs nothing, and it says what the
//!   layout means.
//! - `org.freedesktop.appearance` `color-scheme`, `accent-color` and `contrast` -- from the
//!   session's palette, so a libadwaita application picks the same light or dark as the chrome
//!   around it.
//!
//! Everything else answers `NotFound`, which is not a gap but the point. Claiming an interface
//! in `wlrix-portals.conf` takes the *whole* interface from the fallback backend, so this one is
//! now the only thing a toolkit asks -- and a key it invents an answer for is a key the user can
//! no longer set. `font-name` and the cursor settings are the ones that matter: the cursor is
//! already handed to clients through the session's environment, and the font is the user's.
//! Answering `NotFound` leaves both where they were.
//!
//! ## `SettingChanged`, and the reasoning that got it wrong once
//!
//! It is emitted, on `SIGHUP` from `wlrix-settings-daemon` -- see [`announce`] and
//! [`crate::pidfile`].
//!
//! It deliberately was not, at first, and the argument was this: the colors themselves reach GTK
//! through `~/.config/gtk-{3,4}.0/gtk.css`, and **GTK does not reload that file when it
//! changes** (measured -- a window left running across a change reported the same headerbar
//! color ten seconds later), so firing the signal would flip the dark hint on a window whose
//! CSS half stayed on the old scheme.
//!
//! That weighs a real cost and misses a larger one, because it reasons about GTK only. **A
//! toolkit reads these settings once, at startup, and learns about every later change from this
//! signal and nothing else.** Not emitting it does not leave an application half-updated; it
//! leaves every already-running application wrong indefinitely, with the correct answer sitting
//! on the bus the whole time. That is how Edge stayed in light mode under a dark scheme: it had
//! started 53 seconds before the scheme moved, and nothing ever told it.
//!
//! Chromium, Electron and Qt follow this signal and have no other source, so for them it is the
//! whole mechanism. For GTK the original objection stands and is now the smaller of the two
//! problems: a running GTK window flips its dark hint while its `gtk.css` colors stay put. A
//! GTK 3 module could close that (KDE's `colorreload-gtk-module` is the route), and GTK 4 has no
//! modules at all.
//!
//! ## One layer of variant, not two
//!
//! `Read` returns the value wrapped once. The famous double wrapping is the *frontend's*
//! deprecated `org.freedesktop.portal.Settings.Read`, which adds a layer of its own when it
//! proxies this one; its own XML documents that as a mistake it cannot now correct. There is no
//! `ReadOne` here because the impl interface does not have one -- that too is frontend-only.

use std::collections::HashMap;

use zbus::fdo;
use zbus::zvariant::{OwnedValue, Value};

use crate::config::Config;

/// The namespace a toolkit reads its window-button order from.
const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
/// The cross-desktop appearance namespace: light or dark, and the accent color.
const APPEARANCE: &str = "org.freedesktop.appearance";

/// Where IRIX put the window buttons.
///
/// The menu button on the left, then minimize and maximize on the right. No close: IRIX kept
/// Close in the window menu, which is what the menu button opens, and
/// `wlrix-compositor`'s `FrameStyle` has exactly these three.
const BUTTON_LAYOUT: &str = "menu:minimize,maximize";

/// The backend's interface version, as the property reports it.
const VERSION: u32 = 1;

/// This interface's name, for emitting the signal by hand. The `#[zbus(signal)]` method
/// above needs a `SignalEmitter`, which belongs to the object server on the bus thread; the
/// loop has only the connection, so it sends the message itself the way `Session.Closed`
/// does.
const INTERFACE: &str = "org.freedesktop.impl.portal.Settings";

pub struct Settings;

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl Settings {
    /// Every setting in the namespaces asked for.
    ///
    /// An empty list, or an empty string in the list, matches everything. A trailing `*` globs,
    /// and only there — `org.example.*` is a pattern and `org.*.example` is not, which is what
    /// the interface's own documentation says.
    fn read_all(
        &self,
        namespaces: Vec<String>,
    ) -> fdo::Result<HashMap<String, HashMap<String, OwnedValue>>> {
        let settings = current();
        let mut out: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
        for (namespace, key, value) in settings {
            if !matches_any(&namespaces, namespace) {
                continue;
            }
            out.entry(namespace.to_owned())
                .or_default()
                .insert(key.to_owned(), value);
        }
        Ok(out)
    }

    /// One setting, wrapped in a single layer of variant.
    ///
    /// An unknown namespace or key is an error rather than an empty value, which the interface
    /// requires — and which is what leaves a toolkit free to fall back to its own source for
    /// everything this backend does not own.
    fn read(&self, namespace: &str, key: &str) -> fdo::Result<OwnedValue> {
        current()
            .into_iter()
            .find(|(ns, k, _)| *ns == namespace && *k == key)
            .map(|(_, _, value)| value)
            .ok_or_else(|| {
                fdo::Error::UnknownProperty(format!("no such setting: {namespace} {key}"))
            })
    }

    /// Emitted when a setting this backend serves takes a new value.
    ///
    /// Fired by [`announce`] when the session's scheme moves, which is the only way a
    /// running application learns of it: a toolkit reads the settings once, at startup.
    #[zbus(signal)]
    async fn setting_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Value<'_>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn version(&self) -> u32 {
        VERSION
    }
}

/// Whether `namespace` is one of the patterns asked for.
///
/// Empty list or empty pattern matches everything, per the interface. Globbing is trailing-only.
fn matches_any(patterns: &[String], namespace: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|pattern| match pattern.as_str() {
        "" => true,
        pattern => match pattern.strip_suffix('*') {
            Some(prefix) => namespace.starts_with(prefix),
            None => pattern == namespace,
        },
    })
}

/// Every setting this backend serves, right now.
///
/// The config is re-read on each call rather than cached. Reads happen when an application
/// starts, so this is a handful of them a minute at worst, and it means an application launched
/// after a scheme change is told the new one without this process having to be told anything.
fn current() -> Vec<(&'static str, &'static str, OwnedValue)> {
    let config = Config::load();
    settings_for(&config)
}

/// Split from [`current`] so the table can be tested without a config file on disk.
fn settings_for(config: &Config) -> Vec<(&'static str, &'static str, OwnedValue)> {
    let (palette, unknown) = wlrix_ui::palette::resolve(config.appearance.palette.as_deref());
    if let Some(why) = unknown {
        tracing::warn!("{why}; reporting {}", palette.id);
    }

    let (r, g, b) = palette.accent.channels();
    let accent = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );

    vec![
        (WM_PREFERENCES, "button-layout", variant(BUTTON_LAYOUT)),
        // 1 is "prefer dark", 2 is "prefer light". Never 0: wlRIX always has a scheme, and "no
        // preference" would leave a toolkit to guess at something this session knows.
        (
            APPEARANCE,
            "color-scheme",
            variant(if palette.dark { 1u32 } else { 2u32 }),
        ),
        (APPEARANCE, "accent-color", variant(accent)),
        // 0 is normal contrast. IRIX schemes are not high-contrast variants of each other --
        // Gotham is a dark scheme, not an accessibility one.
        (APPEARANCE, "contrast", variant(0u32)),
    ]
}

/// One value, wrapped for the wire.
fn variant<'a, T: Into<Value<'a>>>(value: T) -> OwnedValue {
    // Every type used here is a plain scalar or a tuple of them, none of which can fail to
    // convert; the fallible signature is `OwnedValue`'s in general, not this call's.
    OwnedValue::try_from(value.into()).expect("a scalar is always a value")
}

/// Tell every application what moved between two configs.
///
/// Called from the main loop on `SIGHUP`, after `portal.toml` has been re-read. Only keys whose
/// value actually differs are announced: `button-layout` is the same string under every scheme,
/// and a toolkit told its window buttons had changed would relayout its titlebar for nothing.
///
/// Failures are logged and swallowed. The value on disk is right either way, and every
/// application started after this point reads it correctly; a signal that could not be sent
/// costs the running ones their update and nothing more.
pub fn announce(connection: &zbus::blocking::Connection, before: &Config, after: &Config) {
    for (namespace, key, value) in changed(&settings_for(before), &settings_for(after)) {
        tracing::info!(namespace, key, "announcing {value:?}");
        if let Err(err) = connection.emit_signal(
            None::<&str>,
            crate::dbus::OBJECT_PATH,
            INTERFACE,
            "SettingChanged",
            // `(s, s, v)`: `OwnedValue` carries its own signature, so the third member is the
            // variant the interface asks for rather than the value inside it.
            &(namespace, key, &value),
        ) {
            tracing::warn!(namespace, key, "could not announce the change: {err}");
        }
    }
}

/// The entries of `after` that `before` did not have, or had differently.
///
/// Split out so the diff can be tested without a bus. A key that vanished is not reported: this
/// backend serves a fixed table, so it cannot happen, and the interface has no way to say a
/// setting is gone.
fn changed<'a>(
    before: &[(&'a str, &'a str, OwnedValue)],
    after: &[(&'a str, &'a str, OwnedValue)],
) -> Vec<(&'a str, &'a str, OwnedValue)> {
    after
        .iter()
        .filter(|(namespace, key, value)| {
            !before
                .iter()
                .any(|(ns, k, old)| ns == namespace && k == key && old == value)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(palette: Option<&str>) -> Config {
        Config {
            appearance: crate::config::Appearance {
                palette: palette.map(ToOwned::to_owned),
            },
            ..Default::default()
        }
    }

    fn read(config: &Config, namespace: &str, key: &str) -> Option<OwnedValue> {
        settings_for(config)
            .into_iter()
            .find(|(ns, k, _)| *ns == namespace && *k == key)
            .map(|(_, _, value)| value)
    }

    #[test]
    fn the_button_layout_is_the_frames_own() {
        // The whole reason this interface exists. `wlrix-compositor`'s FrameStyle has a menu
        // button, minimize and maximize and no close, and a GTK window beside a server-decorated
        // one has to show the same three or the desktop reads as two different window managers.
        let value = read(&config(None), WM_PREFERENCES, "button-layout").expect("served");
        assert_eq!(String::try_from(value).unwrap(), "menu:minimize,maximize");
    }

    #[test]
    fn the_layout_does_not_depend_on_the_scheme() {
        // It is the frame's shape, not its colors. A scheme that changed it would mean the
        // buttons moved when somebody picked a different gray.
        for scheme in [None, Some("classic"), Some("gotham"), Some("nonesuch")] {
            let value = read(&config(scheme), WM_PREFERENCES, "button-layout").expect("served");
            assert_eq!(
                String::try_from(value).unwrap(),
                "menu:minimize,maximize",
                "{scheme:?}"
            );
        }
    }

    #[test]
    fn a_dark_scheme_asks_for_a_dark_toolkit() {
        // 1 is "prefer dark", 2 is "prefer light".
        let dark = read(&config(Some("gotham")), APPEARANCE, "color-scheme").expect("served");
        assert_eq!(u32::try_from(dark).unwrap(), 1);
        let light = read(&config(Some("classic")), APPEARANCE, "color-scheme").expect("served");
        assert_eq!(u32::try_from(light).unwrap(), 2);
    }

    #[test]
    fn an_unknown_scheme_falls_back_rather_than_saying_nothing() {
        // `resolve` answers the default and hands back the reason. A toolkit told "no
        // preference" because of a typo would pick its own light or dark, which is the one
        // outcome this interface exists to prevent.
        let value =
            read(&config(Some("indigo-magic")), APPEARANCE, "color-scheme").expect("served");
        assert_eq!(
            u32::try_from(value).unwrap(),
            2,
            "classic is a light scheme"
        );
    }

    #[test]
    fn the_accent_is_the_palettes_own_in_the_range_the_spec_asks_for() {
        let value = read(&config(Some("classic")), APPEARANCE, "accent-color").expect("served");
        let (r, g, b) = <(f64, f64, f64)>::try_from(value).expect("a (ddd)");
        for channel in [r, g, b] {
            assert!((0.0..=1.0).contains(&channel), "{channel} is out of range");
        }
        // classic's accent is IRIX's checkColor, pure red. Named rather than asserted loosely,
        // because an accent that came out gray would still be "in range".
        let expected = wlrix_ui::palette::by_id("classic")
            .unwrap()
            .accent
            .channels();
        assert_eq!(
            (r, g, b),
            (
                f64::from(expected.0) / 255.0,
                f64::from(expected.1) / 255.0,
                f64::from(expected.2) / 255.0
            )
        );
    }

    #[test]
    fn nothing_else_is_answered_for() {
        // Claiming this interface takes the whole of it from the fallback backend, so a key
        // invented here is a key the user can no longer set in settings.ini. The font and the
        // cursor are the ones that would hurt.
        let all = settings_for(&config(None));
        for (namespace, key) in [
            ("org.gnome.desktop.interface", "font-name"),
            ("org.gnome.desktop.interface", "cursor-theme"),
            ("org.gnome.desktop.interface", "cursor-size"),
            ("org.gnome.desktop.interface", "gtk-theme"),
            ("org.gnome.desktop.interface", "icon-theme"),
        ] {
            assert!(
                !all.iter().any(|(ns, k, _)| *ns == namespace && *k == key),
                "{namespace} {key} is answered for and should not be"
            );
        }
    }

    #[test]
    fn an_empty_namespace_list_matches_everything() {
        assert!(matches_any(&[], APPEARANCE));
        assert!(matches_any(&[String::new()], APPEARANCE));
    }

    #[test]
    fn globbing_is_trailing_only() {
        let glob = |p: &str| matches_any(&[p.to_owned()], "org.freedesktop.appearance");
        assert!(glob("org.freedesktop.appearance"));
        assert!(glob("org.freedesktop.*"));
        assert!(glob("org.*"));
        assert!(!glob("org.gnome.*"));
        // Not a pattern the interface defines, so it is matched literally and finds nothing --
        // which is better than inventing a wildcard nobody asked for.
        assert!(!glob("org.*.appearance"));
    }

    #[test]
    fn a_scheme_change_announces_the_colors_and_not_the_layout() {
        // The bug this exists to prevent, from both ends. A toolkit that is not told stays on
        // the scheme it started with -- which is how Edge sat in light mode under gotham. A
        // toolkit told the *button layout* changed would relayout its titlebar for nothing,
        // since that string is the same under every scheme.
        let moved = changed(
            &settings_for(&config(Some("classic"))),
            &settings_for(&config(Some("gotham"))),
        );
        let keys: Vec<_> = moved.iter().map(|(ns, key, _)| (*ns, *key)).collect();
        assert!(
            keys.contains(&(APPEARANCE, "color-scheme")),
            "the dark hint has to be announced: {keys:?}"
        );
        assert!(
            !keys.contains(&(WM_PREFERENCES, "button-layout")),
            "the layout does not change with the scheme: {keys:?}"
        );
    }

    #[test]
    fn a_reload_that_changed_nothing_announces_nothing() {
        // `wlrix-settings-daemon` signals every owner of a group key, so this backend is
        // SIGHUPed whenever any component's palette is written -- including when the value it
        // reads is the one it already had.
        let same = config(Some("gotham"));
        assert!(changed(&settings_for(&same), &settings_for(&same)).is_empty());
    }

    #[test]
    fn the_diff_is_per_key_rather_than_per_scheme() {
        // Two schemes can share their darkness and differ in accent, and an application told
        // only about `color-scheme` would keep the old one. Written against the function rather
        // than against two real schemes because every scheme in the catalog currently carries
        // IRIX red, so no pair of them would exercise this.
        let before = vec![
            (APPEARANCE, "color-scheme", variant(2u32)),
            (APPEARANCE, "accent-color", variant((1.0f64, 0.0, 0.0))),
        ];
        let after = vec![
            (APPEARANCE, "color-scheme", variant(2u32)),
            (APPEARANCE, "accent-color", variant((0.0f64, 0.0, 1.0))),
        ];
        let keys: Vec<_> = changed(&before, &after)
            .iter()
            .map(|(ns, key, _)| (*ns, *key))
            .collect();
        assert_eq!(keys, [(APPEARANCE, "accent-color")]);
    }

    #[test]
    fn the_gamma_variants_report_the_same_settings() {
        // Not a defect: gamma is how a scheme is rendered, and none of the three roles this
        // interface reports moves with it. So switching between them announces nothing, and a
        // toolkit is right to hear nothing.
        for other in ["classic-g10", "classic-g24"] {
            assert!(
                changed(
                    &settings_for(&config(Some("classic"))),
                    &settings_for(&config(Some(other))),
                )
                .is_empty(),
                "classic and {other} differ here"
            );
        }
    }

    #[test]
    fn read_all_groups_by_namespace() {
        let settings = settings_for(&config(Some("gotham")));
        let mut appearance = 0;
        let mut wm = 0;
        for (namespace, _, _) in &settings {
            match *namespace {
                APPEARANCE => appearance += 1,
                WM_PREFERENCES => wm += 1,
                other => panic!("unexpected namespace {other}"),
            }
        }
        assert_eq!(wm, 1);
        assert_eq!(appearance, 3, "color-scheme, accent-color and contrast");
    }
}
