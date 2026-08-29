// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.Screenshot`: the interface `xdg-desktop-portal` hands a
//! screenshot request to.
//!
//! One call that matters, and it is far simpler than [`super::screencast`]'s three: there is no
//! session, no PipeWire, and nothing to keep running afterwards. Take a picture, write it
//! somewhere the frontend can read it, answer with the URI.
//!
//! ## Where the pixels come from
//!
//! Not from this process. `wlrix-screenshot` is spawned for the job, exactly as
//! `wlrix-source-picker` is spawned to ask which screen to share -- one shape of helper, one
//! shape of contract, and no second implementation of screen capture. It also means the file
//! naming, the region overlay and the PNG encoder all live in the program that already has
//! them; see [`crate::shot`].
//!
//! ## Why `AvailableTargets` is not all four
//!
//! `Screen` and `Area` only. `Window` means "a window the user picks", which needs a window
//! picker the tool does not have, and `ActiveWindow` needs the focused window's *frame*
//! rectangle -- which no client can work out, because the compositor draws wlRIX's 4Dwm frames
//! outside the window's own surface tree. The compositor hands that rectangle to
//! `wlrix-screenshot` on the command line for Alt+Print, and nothing reaches it from here.
//!
//! Leaving them out is the same discipline `AvailableSourceTypes` applies to `VIRTUAL` and
//! `AvailableCursorModes` to `METADATA`: the frontend offers an application only what actually
//! works.

use std::collections::HashMap;

use zbus::{
    ObjectServer,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use super::{PortalResponse, Request, oneshot, request};

/// What can be shot, as the `AvailableTargets` bitmask. See the module note on the other two.
const TARGET_SCREEN: u32 = 1;
const TARGET_AREA: u32 = 4;
const AVAILABLE_TARGETS: u32 = TARGET_SCREEN | TARGET_AREA;

/// Which revision of the interface this implements.
///
/// 3, the revision that added `target` and `AvailableTargets` -- both of which are real here.
/// Version 2 added the `permission_store_checked` hint, which is the frontend's business and
/// needs nothing of a backend.
const VERSION: u32 = 3;

/// What `Screenshot` was asked for, carried to the main loop.
#[derive(Debug, Clone, Default)]
pub struct ShotOptions {
    /// Whether the user gets to choose an area. The interface's hint, defaulting to no.
    pub interactive: bool,
    /// One of the targets above, or `0` for "the caller did not say".
    pub target: u32,
}

impl ShotOptions {
    /// Read the options vardict, ignoring anything unrecognized.
    ///
    /// Lenient on purpose, and the opposite of how wlRIX reads its *config* files: an unknown
    /// key there is a typo worth an error, but here it is a newer frontend passing an option
    /// from a later revision of the interface, and refusing the call would break screenshots
    /// on an upgrade that changed nothing about wlRIX.
    fn parse(options: &HashMap<String, OwnedValue>) -> Self {
        let mut parsed = Self::default();
        if let Some(interactive) = options
            .get("interactive")
            .and_then(|v| bool::try_from(v).ok())
        {
            parsed.interactive = interactive;
        }
        if let Some(target) = options.get("target").and_then(|v| u32::try_from(v).ok()) {
            // Only what was advertised. A target outside the mask is a frontend offering the
            // application something this backend did not claim; treated as unsaid, which lands
            // on the area overlay -- the answer closest to every target there is.
            parsed.target = target & AVAILABLE_TARGETS;
        }
        parsed
    }
}

pub struct Screenshot {
    pub sender: calloop::channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    /// Take a screenshot and answer with where it went.
    async fn screenshot(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let parsed = ShotOptions::parse(&options);
        tracing::debug!(app_id = %app_id, request = %handle, parent_window = %parent_window, ?parsed, "Screenshot");

        // The Request object is how the application cancels while the overlay is up. Exported
        // before the loop starts on anything slow, and removed however this ends -- the same
        // shape `screencast::start` uses, and for the same reason.
        let request_object = request::PortalRequest {
            path: handle.clone(),
            sender: self.sender.clone(),
        };
        if let Err(err) = server.at(&handle, request_object).await {
            tracing::error!("could not export the request object: {err}");
            return (PortalResponse::Ended as u32, HashMap::new());
        }

        let (reply, receiver) = oneshot();
        let request = handle.clone();
        // Awaiting rather than blocking: an interactive shot waits on a person, and a blocked
        // handler would hold zbus's executor and stop `Request.Close` being dispatched -- so
        // cancelling would only take effect after the thing it cancels had finished. See the
        // [`super`] module documentation.
        let (response, uri) = if self
            .sender
            .send(Request::Screenshot {
                request,
                app_id,
                options: parsed,
                reply,
            })
            .is_err()
        {
            tracing::error!("the main loop is gone; failing the call");
            (PortalResponse::Ended, String::new())
        } else {
            receiver
                .recv()
                .await
                .unwrap_or((PortalResponse::Ended, String::new()))
        };

        let _ = server.remove::<request::PortalRequest, _>(&handle).await;

        if response != PortalResponse::Success {
            return (response as u32, HashMap::new());
        }
        let results = HashMap::from([(
            "uri".to_string(),
            OwnedValue::try_from(Value::from(uri.as_str()))
                .expect("a string is always representable as an OwnedValue"),
        )]);
        (PortalResponse::Success as u32, results)
    }

    /// Read the color of one pixel.
    ///
    /// Not implemented, and answered honestly rather than with a made-up color. It needs a
    /// crosshair-and-loupe mode in `wlrix-screenshot` — the overlay already holds the frozen
    /// pixels, so it is a small thing, but it is a second interaction and it is not built.
    ///
    /// The method exists because the interface defines it: a backend that serves the interface
    /// serves all of it, and a frontend calling a method that is simply absent gets a D-Bus
    /// error rather than a portal response the application knows how to show.
    async fn pick_color(
        &self,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::info!(app_id = %app_id, request = %handle, "PickColor is not implemented");
        (PortalResponse::Ended as u32, HashMap::new())
    }

    #[zbus(property)]
    fn available_targets(&self) -> u32 {
        AVAILABLE_TARGETS
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.try_clone().expect("cloneable")))
            .collect()
    }

    #[test]
    fn the_defaults_are_a_plain_shot() {
        let parsed = ShotOptions::parse(&HashMap::new());
        assert!(!parsed.interactive);
        assert_eq!(parsed.target, 0);
    }

    #[test]
    fn the_advertised_targets_come_through() {
        let parsed = ShotOptions::parse(&options(&[("target", OwnedValue::from(TARGET_AREA))]));
        assert_eq!(parsed.target, TARGET_AREA);
    }

    /// A target this backend never claimed reads as "the caller did not say", which lands on
    /// the area overlay rather than failing the call.
    #[test]
    fn an_unadvertised_target_is_dropped() {
        // 8 is ActiveWindow, which is not in `AvailableTargets`.
        let parsed = ShotOptions::parse(&options(&[("target", OwnedValue::from(8u32))]));
        assert_eq!(parsed.target, 0);
    }

    /// A key from a later revision of the interface must not break a screenshot.
    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = ShotOptions::parse(&options(&[
            ("interactive", OwnedValue::from(true)),
            ("something_new", OwnedValue::from(1u32)),
        ]));
        assert!(parsed.interactive);
    }
}
