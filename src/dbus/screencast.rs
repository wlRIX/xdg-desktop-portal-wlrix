// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.ScreenCast`: the interface `xdg-desktop-portal` hands a
//! screen-sharing request to.
//!
//! Three calls, always in this order, and the frontend creates the object paths:
//!
//! 1. `CreateSession` -- a session begins. We export an
//!    [`org.freedesktop.impl.portal.Session`][super::session] object at the path we are given,
//!    which is how either side ends it later.
//! 2. `SelectSources` -- what the application wants: monitors, windows, or either; one source
//!    or several; whether the cursor should be in the stream. **No UI here.** The frontend may
//!    call this without ever calling `Start`, and a picker that appeared at this point would
//!    show up for a share nobody went through with.
//! 3. `Start` -- show the picker, capture what was chosen, and answer with the PipeWire nodes.
//!
//! Note what is *not* here: `OpenPipeWireRemote` belongs to the frontend interface, not this
//! one. `xdg-desktop-portal` connects to PipeWire itself and hands the application a remote
//! restricted to the node ids reported below, so this backend never passes a file descriptor.

use std::collections::HashMap;

use zbus::{
    ObjectServer,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use super::{PortalResponse, Request, oneshot, request, session};

/// Content this backend can capture, as the `AvailableSourceTypes` bitmask.
///
/// Monitor and window; not `VIRTUAL` (3), which means a source the compositor creates on demand
/// for the cast -- a headless output. wlRIX has no way to make one.
const SOURCE_TYPE_MONITOR: u32 = 1;
const SOURCE_TYPE_WINDOW: u32 = 2;
const AVAILABLE_SOURCE_TYPES: u32 = SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW;

/// How the pointer can appear, as the `AvailableCursorModes` bitmask.
///
/// `HIDDEN` (1) and `EMBEDDED` (2), which the compositor's capture supports directly -- a
/// session either draws the cursor into the frame or does not.
///
/// `METADATA` (4) is deliberately not offered. It means "send the cursor position and image
/// out of band, alongside the frames", and needs `ext_image_copy_capture_cursor_session_v1`,
/// which wlRIX's compositor does not implement. Advertising it and then not delivering would
/// leave a client drawing no cursor at all, which is worse than telling it up front.
const CURSOR_MODE_HIDDEN: u32 = 1;
const CURSOR_MODE_EMBEDDED: u32 = 2;
const AVAILABLE_CURSOR_MODES: u32 = CURSOR_MODE_HIDDEN | CURSOR_MODE_EMBEDDED;

/// Which revision of the interface this implements.
///
/// 4, not the 6 the installed `xdg-desktop-portal` documents. 5 and 6 are about restore tokens
/// and `pipewire-serial` stream targeting, neither of which is implemented; claiming them would
/// have the frontend offer applications a "share again without asking" that silently never
/// works. 4 is the last revision every part of which is real here.
const VERSION: u32 = 4;

/// What `SelectSources` asked for, carried to the main loop.
#[derive(Debug, Clone)]
pub struct SelectOptions {
    /// Bitmask of [`SOURCE_TYPE_MONITOR`] / [`SOURCE_TYPE_WINDOW`].
    pub types: u32,
    /// Whether the user may choose more than one source.
    pub multiple: bool,
    /// One of the advertised cursor modes.
    pub cursor_mode: u32,
}

impl Default for SelectOptions {
    fn default() -> Self {
        // The defaults the interface documents: monitors only, one of them, no cursor.
        Self {
            types: SOURCE_TYPE_MONITOR,
            multiple: false,
            cursor_mode: CURSOR_MODE_HIDDEN,
        }
    }
}

impl SelectOptions {
    /// Read the options vardict, ignoring anything unrecognized.
    ///
    /// Lenient on purpose, and the opposite of how wlRIX reads its *config* files: an unknown
    /// key there is a typo worth an error, but here it is a newer frontend passing an option
    /// from a later revision of the interface, and refusing the call would break screen sharing
    /// on an upgrade that changed nothing about wlRIX.
    fn parse(options: &HashMap<String, OwnedValue>) -> Self {
        let mut parsed = Self::default();
        if let Some(types) = options.get("types").and_then(|v| u32::try_from(v).ok()) {
            parsed.types = types & AVAILABLE_SOURCE_TYPES;
        }
        if let Some(multiple) = options.get("multiple").and_then(|v| bool::try_from(v).ok()) {
            parsed.multiple = multiple;
        }
        if let Some(mode) = options
            .get("cursor_mode")
            .and_then(|v| u32::try_from(v).ok())
        {
            parsed.cursor_mode = mode;
        }
        parsed
    }

    /// Whether the requested cursor mode is one that was advertised.
    ///
    /// The interface says a mode outside `AvailableCursorModes` must close the session rather
    /// than be quietly substituted, so this is checked rather than clamped.
    fn cursor_mode_is_available(&self) -> bool {
        self.cursor_mode.count_ones() == 1 && self.cursor_mode & AVAILABLE_CURSOR_MODES != 0
    }
}

pub struct ScreenCast {
    pub sender: calloop::channel::Sender<Request>,
}

impl ScreenCast {
    /// Hand a request to the main loop and wait for its answer.
    ///
    /// Awaiting rather than blocking is the point: see the module documentation on
    /// [`super`]. A send that fails means the loop is gone, which is fatal for the call but
    /// not worth panicking over -- the process is on its way out anyway.
    async fn ask<T: Default>(
        &self,
        make: impl FnOnce(super::Reply<T>) -> Request,
        receiver: async_channel::Receiver<(PortalResponse, T)>,
        reply: super::Reply<T>,
    ) -> (PortalResponse, T) {
        if self.sender.send(make(reply)).is_err() {
            tracing::error!("the main loop is gone; failing the call");
            return (PortalResponse::Ended, T::default());
        }
        receiver
            .recv()
            .await
            .unwrap_or((PortalResponse::Ended, T::default()))
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    /// Begin a session. No UI, no capture — just somewhere to hang the rest of it.
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::debug!(app_id = %app_id, session = %session_handle, request = %handle, "CreateSession");

        // The Session object is what either side closes the session through. Exported before
        // the loop is told, so a Close arriving immediately has something to arrive at.
        let session_object = session::Session {
            path: session_handle.clone(),
            sender: self.sender.clone(),
        };
        if let Err(err) = server.at(&session_handle, session_object).await {
            tracing::error!("could not export the session object: {err}");
            return (PortalResponse::Ended as u32, HashMap::new());
        }

        let (reply, receiver) = oneshot();
        let session = session_handle.clone();
        let (response, ()) = self
            .ask(
                |reply| Request::CreateSession {
                    session,
                    app_id,
                    reply,
                },
                receiver,
                reply,
            )
            .await;

        if response != PortalResponse::Success {
            let _ = server.remove::<session::Session, _>(&session_handle).await;
            return (response as u32, HashMap::new());
        }

        // `session_id` is documented as a result of this call. The object path is already
        // unique per session and is what every later call is keyed by, so there is nothing to
        // be gained by inventing a second identifier that means the same thing.
        let results = HashMap::from([(
            "session_id".to_string(),
            OwnedValue::try_from(Value::from(session_handle.as_str()))
                .expect("a string is always representable as an OwnedValue"),
        )]);
        (response as u32, results)
    }

    /// Record what the application wants to capture. Still no UI: see the module docs.
    async fn select_sources(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let parsed = SelectOptions::parse(&options);
        tracing::debug!(app_id = %app_id, session = %session_handle, request = %handle, ?parsed, "SelectSources");

        if !parsed.cursor_mode_is_available() {
            // The interface requires the session to be closed rather than the mode quietly
            // changed, so that a client asking for cursor metadata finds out it cannot have it.
            tracing::warn!(
                cursor_mode = parsed.cursor_mode,
                "unavailable cursor mode; ending the session"
            );
            let _ = self.sender.send(Request::CloseSession {
                session: session_handle,
            });
            return (PortalResponse::Ended as u32, HashMap::new());
        }

        let (reply, receiver) = oneshot();
        let session = session_handle;
        let (response, ()) = self
            .ask(
                |reply| Request::SelectSources {
                    session,
                    options: parsed,
                    reply,
                },
                receiver,
                reply,
            )
            .await;
        (response as u32, HashMap::new())
    }

    /// Show the picker, start capturing what was chosen, and report the PipeWire nodes.
    ///
    /// The only call here that takes real time, and the only one that shows anything.
    async fn start(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::debug!(app_id = %app_id, session = %session_handle, request = %handle, parent_window = %parent_window, "Start");

        // The Request object is how the application cancels while the picker is up. It must be
        // exported before the loop starts on anything slow, and removed however this ends.
        let request_object = request::PortalRequest {
            path: handle.clone(),
            sender: self.sender.clone(),
        };
        if let Err(err) = server.at(&handle, request_object).await {
            tracing::error!("could not export the request object: {err}");
            return (PortalResponse::Ended as u32, HashMap::new());
        }

        let (reply, receiver) = oneshot();
        let (session, request) = (session_handle, handle.clone());
        let (response, streams) = self
            .ask(
                |reply| Request::Start {
                    session,
                    request,
                    app_id,
                    parent_window,
                    reply,
                },
                receiver,
                reply,
            )
            .await;

        let _ = server.remove::<request::PortalRequest, _>(&handle).await;

        if response != PortalResponse::Success {
            return (response as u32, HashMap::new());
        }
        let results = HashMap::from([(
            "streams".to_string(),
            OwnedValue::try_from(Value::from(streams)).unwrap_or_else(|err| {
                // Only reachable if a stream's property dict holds something unrepresentable,
                // which would be this program's own bug rather than anything a caller did.
                tracing::error!("could not encode the stream list: {err}");
                OwnedValue::from(0u32)
            }),
        )]);
        (PortalResponse::Success as u32, results)
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        AVAILABLE_SOURCE_TYPES
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        AVAILABLE_CURSOR_MODES
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        VERSION
    }
}
