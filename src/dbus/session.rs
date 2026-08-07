// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.Session`: one screen-sharing session, from `CreateSession`
//! until somebody ends it.
//!
//! Exported by [`super::screencast`] at the path the frontend chose, one object per session.
//! Two ways it ends, and both have to work:
//!
//! - the frontend calls `Close` (the application stopped sharing, or exited), which is the
//!   `close` method below;
//! - this backend decides it is over -- the shared window was closed, the monitor unplugged --
//!   and emits `Closed`.
//!
//! The second is why the emitter is kept rather than the object being fire-and-forget: without
//! it, an application whose shared window disappears sits watching a stream that will never
//! carry another frame.

use zbus::{object_server::SignalEmitter, zvariant::OwnedObjectPath};

use super::Request;

pub struct Session {
    /// This object's own path, which is how the main loop knows the session apart.
    pub path: OwnedObjectPath,
    pub sender: calloop::channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    /// The frontend is ending the session.
    ///
    /// Note what this does *not* do: emit `Closed`. That signal means "this backend ended it",
    /// and the caller here already knows — it is the one ending it. Emitting anyway is a
    /// common enough portal bug that it is worth naming.
    fn close(&self) {
        tracing::debug!(session = %self.path, "Session.Close");
        let _ = self.sender.send(Request::CloseSession {
            session: self.path.clone(),
        });
    }

    /// Emitted when the backend ends the session on its own — the captured window went away,
    /// the output was unplugged, the compositor stopped the capture.
    #[zbus(signal)]
    pub async fn closed(
        emitter: &SignalEmitter<'_>,
        details: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}
