// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.Request`: one outstanding call the application can give up on.
//!
//! Only `Start` exports one, because it is the only call that takes long enough for anybody to
//! change their mind: a browser tab closed while the source picker is on screen arrives here.
//! `CreateSession` and `SelectSources` answer immediately, and a Request object for them would
//! be gone before anything could call it.
//!
//! This is the reason the ScreenCast handlers are `async fn` rather than blocking -- see the
//! [`super`] module documentation. A blocked `Start` would not let `Close` be dispatched until
//! `Start` itself had returned, so canceling would only take effect after the thing being
//! canceled had finished.

use zbus::zvariant::OwnedObjectPath;

use super::Request;

pub struct PortalRequest {
    /// The request path the frontend chose, which is what the main loop matches on to find the
    /// picker it should take down.
    pub path: OwnedObjectPath,
    pub sender: calloop::channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl PortalRequest {
    /// Give up on the call.
    ///
    /// This does not answer the outstanding `Start`; it tells the main loop to stop what it is
    /// doing, and the loop answers `Start` itself with [`super::PortalResponse::Canceled`].
    /// One reply, from one place, whichever way the call ended.
    fn close(&self) {
        tracing::debug!(request = %self.path, "Request.Close");
        let _ = self.sender.send(Request::Cancel {
            request: self.path.clone(),
        });
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}
