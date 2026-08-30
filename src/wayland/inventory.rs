// SPDX-License-Identifier: GPL-3.0-or-later
//! What there is to share: the monitors and the windows.
//!
//! Two protocols, kept in one place because the picker shows them side by side and the portal
//! treats them alike once chosen. Monitors come from `wl_output`; windows come from
//! `ext-foreign-toplevel-list-v1`, the compositor's read-only window list.
//!
//! Both are *live*. A monitor unplugged or a window closed while the picker is open has to
//! disappear from it, and -- more importantly -- a source that goes away mid-cast has to stop
//! the stream rather than leave it frozen on a last frame forever.

use wayland_client::protocol::wl_output::WlOutput;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;

/// How a source is named in the picker manifest and in the selection that comes back.
///
/// A string rather than an index: the list can change between the manifest being written and the
/// answer arriving -- a window closes while somebody is reading the dialog -- and an index would
/// then silently mean a different window. A stale id, by contrast, simply matches nothing.
///
/// Monitors are keyed by their `wl_output` name (`DisplayPort-4`), which survives the output
/// being reconfigured. Windows are keyed by the foreign-toplevel `identifier`, which the
/// protocol guarantees is unique and stable for the window's lifetime.
pub type SourceId = String;

/// A monitor.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub output: WlOutput,
    /// `wl_output.name`, e.g. `DisplayPort-4`. The identity the rest of wlRIX uses for an
    /// output too -- it is what `outputs.toml` keys on.
    pub name: String,
    /// `wl_output.description`, e.g. the make and model. What the picker shows a person.
    pub description: String,
    /// Position in the compositor's coordinate space, from `wl_output.geometry`. Reported back
    /// to the application in the stream's `position` property.
    pub position: (i32, i32),
    /// Current mode, in pixels.
    pub size: (i32, i32),
    /// Whether a `done` has been seen. Until then the fields above are still being filled in and
    /// the monitor must not be offered -- a picker tile captioned with an empty string is worse
    /// than one that appears a frame later.
    pub ready: bool,
}

/// A window.
#[derive(Debug, Clone)]
pub struct Window {
    pub handle: ExtForeignToplevelHandleV1,
    /// The protocol's `identifier`: unique, stable, and meaningless to a human.
    pub identifier: String,
    pub title: String,
    pub app_id: String,
    /// Set by `done`, as for [`Monitor`]. The list announces a window before saying anything
    /// about it, so an eager reader sees a window with no title at all.
    pub ready: bool,
}

impl Window {
    /// What to show in the picker.
    ///
    /// The title, because that is how a person tells two windows of the same application apart.
    /// Falling back to the app id, and then to something rather than an empty tile: a window
    /// with neither is rare but not impossible, and an unlabeled tile is unpickable.
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.app_id.is_empty() {
            &self.app_id
        } else {
            "Untitled window"
        }
    }
}

/// Everything that can be captured, as the compositor currently sees it.
#[derive(Default)]
pub struct Inventory {
    pub monitors: Vec<Monitor>,
    pub windows: Vec<Window>,
}

impl Inventory {
    /// The monitors worth offering.
    pub fn ready_monitors(&self) -> impl Iterator<Item = &Monitor> {
        self.monitors.iter().filter(|monitor| monitor.ready)
    }

    /// The windows worth offering.
    pub fn ready_windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(|window| window.ready)
    }

    pub fn monitor(&self, id: &str) -> Option<&Monitor> {
        self.ready_monitors().find(|monitor| monitor.name == id)
    }

    pub fn window(&self, id: &str) -> Option<&Window> {
        self.ready_windows().find(|window| window.identifier == id)
    }

    pub fn monitor_mut(&mut self, output: &WlOutput) -> Option<&mut Monitor> {
        self.monitors
            .iter_mut()
            .find(|monitor| &monitor.output == output)
    }

    pub fn window_mut(&mut self, handle: &ExtForeignToplevelHandleV1) -> Option<&mut Window> {
        self.windows
            .iter_mut()
            .find(|window| &window.handle == handle)
    }
}
