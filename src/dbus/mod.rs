// SPDX-License-Identifier: GPL-3.0-or-later
//! D-Bus, on a thread of its own.
//!
//! This program is a D-Bus service before it is anything else: `xdg-desktop-portal` proxies an
//! application's screen-sharing request to `org.freedesktop.impl.portal.ScreenCast`, and that
//! interface is the whole of the outside world's view of this backend.
//!
//! calloop is resolutely synchronous, so the two are kept apart rather than reconciled: the bus
//! runs on a dedicated thread and reports into the main loop through a `calloop::channel`. That
//! is the shape `wlrix-idle` uses for its inhibit interfaces and `wlrix-greeter` uses for
//! greetd, and it means one owner for all state -- the loop -- with the bus thread holding
//! nothing but a sender.
//!
//! ## Why the interface methods are `async`, unlike `wlrix-idle`'s
//!
//! Every method `wlrix-idle` serves answers from data it already has. These do not: `Start`
//! has to put a picker on screen and wait for a person, which can be a minute. If it *blocked*
//! that whole time it would also block `Request.Close` -- the call an application makes to
//! cancel a share it has already asked for -- because zbus would not dispatch it until the
//! handler holding the executor returned. Cancelling would then only take effect after the
//! thing it cancels had finished, which is the wrong way round.
//!
//! So the handlers are `async fn` that `await` a reply from the main loop. Awaiting yields the
//! executor rather than holding it, and `Close` is serviced while `Start` is still outstanding.
//! The executor is zbus's own, ticked by the blocking connection on this thread; the calloop
//! side never sees it.

mod request;
mod screencast;
mod session;

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub use screencast::SelectOptions;

/// The name `xdg-desktop-portal` looks up, and the name in `data/wlrix.portal`. The two have to
/// agree or the frontend finds nothing.
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.wlrix";
/// Where every portal backend serves its interfaces. Fixed by the frontend, not a choice.
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// A portal call's outcome, as the frontend expects it.
///
/// Numeric on the wire, and the numbers are not arbitrary: an application distinguishes "the
/// user said no" from "this went wrong", and showing an error dialog for a canceled share is a
/// bug users notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PortalResponse {
    Success = 0,
    /// The user canceled -- closed the picker, or picked nothing.
    Canceled = 1,
    /// Ended some other way: nothing to capture, the compositor refused, an internal failure.
    Ended = 2,
}

/// One PipeWire stream as `Start` reports it: node id plus properties.
///
/// The properties carry `size`, `source_type` and -- for a monitor -- `position`. See the
/// `Start` documentation in `org.freedesktop.impl.portal.ScreenCast.xml`.
pub type Stream = (u32, HashMap<String, OwnedValue>);

/// Something the bus thread needs the main loop to do.
///
/// Each variant that has an answer carries the channel to send it back on. The bus thread awaits
/// that; the loop is free to take as long as it needs, which for `Start` means as long as the
/// user takes to choose.
pub enum Request {
    CreateSession {
        session: OwnedObjectPath,
        app_id: String,
        reply: Reply<()>,
    },
    SelectSources {
        session: OwnedObjectPath,
        options: SelectOptions,
        reply: Reply<()>,
    },
    Start {
        session: OwnedObjectPath,
        request: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        reply: Reply<Vec<Stream>>,
    },
    /// The application gave up on a call that is still outstanding.
    Cancel { request: OwnedObjectPath },
    /// The session is over: stop every stream on it and forget it.
    CloseSession { session: OwnedObjectPath },
}

/// Where a request's answer goes.
///
/// Capacity one and never awaited on send: the loop answers exactly once and must not be able
/// to stall on a caller that has gone away.
pub struct Reply<T>(async_channel::Sender<(PortalResponse, T)>);

impl<T> Reply<T> {
    pub fn send(self, response: PortalResponse, results: T) {
        // An error here means the caller stopped waiting -- it timed out or disconnected. That
        // is not this side's problem, and nothing can be done about it.
        let _ = self.0.try_send((response, results));
    }

    /// Answer with a failure and no results.
    pub fn fail(self, response: PortalResponse)
    where
        T: Default,
    {
        self.send(response, T::default());
    }
}

/// Make a request/reply pair for one call.
fn oneshot<T>() -> (Reply<T>, async_channel::Receiver<(PortalResponse, T)>) {
    let (sender, receiver) = async_channel::bounded(1);
    (Reply(sender), receiver)
}

/// What the main loop keeps hold of: the connection, kept alive.
///
/// Dropping it closes the bus connection and releases the name, so it is held for the life of
/// the process even though nothing calls into it.
pub struct Bus {
    _connection: zbus::blocking::Connection,
}

/// Own the bus name and serve the ScreenCast interface.
///
/// Fails rather than carrying on if the name cannot be had. A portal backend that is running but
/// unreachable is worse than one that is not running: D-Bus activation would keep pointing
/// applications at it, and every screen share would hang instead of falling through to another
/// backend.
pub fn spawn(replace: bool) -> Result<(Bus, calloop::channel::Channel<Request>), String> {
    let (sender, channel) = calloop::channel::channel();

    let connection = zbus::blocking::connection::Builder::session()
        .map_err(|err| format!("no session bus: {err}"))?
        // The object is served before the name is requested, so a frontend that resolves the
        // name the instant it appears never finds an empty connection behind it. That is also
        // why the name is not requested through the builder: it would be taken before this
        // point, leaving a window in which the interface is not there yet.
        .serve_at(OBJECT_PATH, screencast::ScreenCast { sender })
        .map_err(|err| format!("could not serve the ScreenCast interface: {err}"))?
        .build()
        .map_err(|err| format!("could not connect to the session bus: {err}"))?;

    // `DoNotQueue`: queueing would leave this process running and reachable at its unique name
    // but not at the well-known one, so the frontend would keep using whoever holds it while
    // this sat waiting for a turn that may never come. Better to fail and let activation retry.
    let mut flags = zbus::fdo::RequestNameFlags::DoNotQueue.into();
    if replace {
        flags |= zbus::fdo::RequestNameFlags::ReplaceExisting;
    }
    let reply = connection
        .request_name_with_flags(BUS_NAME, flags)
        .map_err(|err| format!("could not request {BUS_NAME}: {err}"))?;
    if !matches!(
        reply,
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner
    ) {
        return Err(format!(
            "{BUS_NAME} is already owned by another portal backend ({reply:?}); \
             pass --replace to take it"
        ));
    }

    Ok((
        Bus {
            _connection: connection,
        },
        channel,
    ))
}
