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
mod screenshot;
mod session;

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub use screencast::SelectOptions;
pub use screenshot::ShotOptions;

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
    /// Take a screenshot. No session: unlike a cast, it is over when the file is written.
    Screenshot {
        request: OwnedObjectPath,
        app_id: String,
        options: ShotOptions,
        /// The `file://` URI of what was written.
        reply: Reply<String>,
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
/// the process.
pub struct Bus {
    /// Also how the loop talks *back* to an application.
    ///
    /// Almost everything here is request/response and answers through a [`Reply`], but
    /// `Session.Closed` is not a reply to anything -- it is the backend saying, unprompted,
    /// that a share has ended. `emit_signal` is synchronous and the connection is `Clone` and
    /// thread-safe, so the loop can send it without going near the bus thread.
    pub connection: zbus::blocking::Connection,
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
        .serve_at(
            OBJECT_PATH,
            screencast::ScreenCast {
                sender: sender.clone(),
            },
        )
        .map_err(|err| format!("could not serve the ScreenCast interface: {err}"))?
        // Two interfaces at one object path, which is what every portal backend does -- the
        // frontend looks each one up by name on the same object.
        .serve_at(OBJECT_PATH, screenshot::Screenshot { sender })
        .map_err(|err| format!("could not serve the Screenshot interface: {err}"))?
        .build()
        .map_err(|err| format!("could not connect to the session bus: {err}"))?;

    // `DoNotQueue`: queueing would leave this process running and reachable at its unique name
    // but not at the well-known one, so the frontend would keep using whoever holds it while
    // this sat waiting for a turn that may never come. Better to fail and let activation retry.
    //
    // `AllowReplacement` **always**, and it is not optional the way it looks. D-Bus replacement
    // is granted by the *incumbent*, not taken by the newcomer: a name requested without this
    // flag can never be replaced, so `--replace` on a later run fails with "name already taken"
    // no matter what that run asks for. Setting it only when `--replace` was passed -- the
    // obvious reading -- makes the flag protect the wrong process and never work.
    let mut flags =
        zbus::fdo::RequestNameFlags::DoNotQueue | zbus::fdo::RequestNameFlags::AllowReplacement;
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
             pass --replace to take it. If --replace was passed and this still failed, the \
             process holding the name predates AllowReplacement and has to be stopped by hand."
        ));
    }

    Ok((Bus { connection }, channel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::object_server::Interface;

    /// The introspection XML an interface actually serves.
    fn introspect(render: impl FnOnce(&mut String)) -> String {
        let mut xml = String::new();
        render(&mut xml);
        xml
    }

    /// Every `<!-- ... -->` in `xml`, as the XML parser would delimit them.
    fn comments(xml: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut rest = xml;
        while let Some(open) = rest.find("<!--") {
            let body = &rest[open + 4..];
            let Some(close) = body.find("-->") else {
                // An unterminated comment is its own kind of broken, and the assertion below
                // is not the place to discover it.
                break;
            };
            found.push(&body[..close]);
            rest = &body[close + 3..];
        }
        found
    }

    /// `--` is forbidden inside an XML comment, and zbus writes the `///` doc comments on this
    /// backend's interface methods into one **verbatim** -- no escaping, see `to_xml_docs` in
    /// zbus_macros. The workspace writes its em-dashes as `--`, which everywhere else is fine
    /// and here produces introspection that no strict XML parser will read.
    ///
    /// That is not a cosmetic defect: introspection is what a client's proxy generator consumes,
    /// so the backend would be undescribable to anything that generates bindings from it. It was
    /// found in `wlrix-settings-daemon`, whose C# client could not be generated until it was
    /// fixed; this backend had the same latent problem, and nothing had tripped over it only
    /// because nothing generates a proxy from it yet.
    ///
    /// The rendered XML is checked rather than the source text, so this cannot be fooled by
    /// where the comment happens to be written.
    #[test]
    fn no_interface_doc_comment_can_break_the_introspection_xml() {
        let (sender, _channel) = calloop::channel::channel::<Request>();
        let path = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/test")
            .expect("a valid object path");

        let screencast = screencast::ScreenCast {
            sender: sender.clone(),
        };
        let screenshot = screenshot::Screenshot {
            sender: sender.clone(),
        };
        let session = session::Session {
            path: path.clone(),
            sender: sender.clone(),
        };
        let request = request::PortalRequest { path, sender };

        let interfaces = [
            (
                "ScreenCast",
                introspect(|xml| screencast.introspect_to_writer(xml, 0)),
            ),
            (
                "Screenshot",
                introspect(|xml| screenshot.introspect_to_writer(xml, 0)),
            ),
            (
                "Session",
                introspect(|xml| session.introspect_to_writer(xml, 0)),
            ),
            (
                "Request",
                introspect(|xml| request.introspect_to_writer(xml, 0)),
            ),
        ];

        for (name, xml) in &interfaces {
            // A sanity check on the test itself: an interface that rendered nothing would pass
            // the real assertion trivially.
            assert!(xml.contains("<interface"), "{name} introspected to nothing");
            for comment in comments(xml) {
                assert!(
                    !comment.contains("--"),
                    "{name}: this doc comment makes the introspection XML unparseable:\n{comment}"
                );
            }
        }
    }

    #[test]
    fn the_comment_scanner_finds_what_it_should() {
        assert_eq!(comments("<node/>"), Vec::<&str>::new());
        assert_eq!(comments("<!-- one --><a/><!-- two -->"), [" one ", " two "]);
        // The shape zbus emits: an open, the lines, then a space before the close.
        assert_eq!(comments("<!--\n a line\n -->"), ["\n a line\n "]);
    }
}
