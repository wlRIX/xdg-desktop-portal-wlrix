// SPDX-License-Identifier: GPL-3.0-or-later
//! The state behind the portal, and the only thing that owns any of it.
//!
//! Everything the backend knows lives here and is touched only from the calloop thread: the
//! sessions, and later the Wayland capture handles, the PipeWire streams, and the running
//! picker. The bus thread holds a sender and nothing else, so there is no shared mutable state
//! between the two and no lock to get wrong -- the same division `wlrix-idle` draws, for the
//! same reason.

use std::collections::HashMap;

use zbus::zvariant::OwnedObjectPath;

use crate::dbus::{PortalResponse, Reply, Request, SelectOptions, Stream};

/// One screen-sharing session, from `CreateSession` to `Close`.
pub struct Session {
    /// Who asked. Empty for an unsandboxed application, which is most of them.
    pub app_id: String,
    /// What `SelectSources` asked for. `None` until it is called -- and a `Start` before that
    /// is a frontend bug, not something to guess a default for.
    pub options: Option<SelectOptions>,
    /// The `Start` that is waiting on the user, if one is. Held so that a `Close` arriving
    /// mid-pick can answer it rather than leaving the frontend waiting forever.
    pub pending_start: Option<PendingStart>,
}

/// A `Start` call that has been asked but not answered.
pub struct PendingStart {
    /// The Request object path, which is what `Request.Close` cancels by.
    pub request: OwnedObjectPath,
    pub reply: Reply<Vec<Stream>>,
}

#[derive(Default)]
pub struct Portal {
    sessions: HashMap<OwnedObjectPath, Session>,
}

impl Portal {
    pub fn handle(&mut self, request: Request) {
        match request {
            Request::CreateSession {
                session,
                app_id,
                reply,
            } => self.create_session(session, app_id, reply),
            Request::SelectSources {
                session,
                options,
                reply,
            } => self.select_sources(session, options, reply),
            Request::Start {
                session,
                request,
                app_id,
                parent_window,
                reply,
            } => self.start(session, request, app_id, parent_window, reply),
            Request::Cancel { request } => self.cancel(request),
            Request::CloseSession { session } => self.close_session(session),
        }
    }

    fn create_session(&mut self, path: OwnedObjectPath, app_id: String, reply: Reply<()>) {
        self.sessions.insert(
            path.clone(),
            Session {
                app_id,
                options: None,
                pending_start: None,
            },
        );
        tracing::info!(session = %path, count = self.sessions.len(), "session created");
        reply.send(PortalResponse::Success, ());
    }

    fn select_sources(&mut self, path: OwnedObjectPath, options: SelectOptions, reply: Reply<()>) {
        let Some(session) = self.sessions.get_mut(&path) else {
            tracing::warn!(session = %path, "SelectSources for a session that does not exist");
            reply.fail(PortalResponse::Ended);
            return;
        };
        session.options = Some(options);
        reply.send(PortalResponse::Success, ());
    }

    fn start(
        &mut self,
        path: OwnedObjectPath,
        request: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        reply: Reply<Vec<Stream>>,
    ) {
        let Some(session) = self.sessions.get_mut(&path) else {
            tracing::warn!(session = %path, "Start for a session that does not exist");
            reply.fail(PortalResponse::Ended);
            return;
        };
        let Some(options) = session.options.clone() else {
            // The frontend is supposed to call SelectSources first. Without it there is nothing
            // to show a picker for -- not even whether to offer windows.
            tracing::warn!(session = %path, "Start before SelectSources");
            reply.fail(PortalResponse::Ended);
            return;
        };

        // `parent_window` is recorded but cannot be honored: it is a `wayland:<handle>` for
        // xdg-foreign, and Avalonia's Wayland backend implements the *export* half of that
        // protocol but not the import half, so the picker cannot parent itself to the window
        // that asked. The picker is an unparented dialog until that lands upstream.
        tracing::info!(
            session = %path,
            request = %request,
            app_id = %app_id,
            parent_window = %parent_window,
            ?options,
            "Start: choosing a source",
        );

        // Nothing captures yet: the picker, the capture path and PipeWire are the next pieces.
        // Until they exist this answers at once, so `pending_start` is not set -- there is no
        // window in which a cancel could arrive. It becomes the normal path the moment `Start`
        // starts waiting on a person.
        //
        // Failing rather than returning an empty success is deliberate: an application told a
        // share succeeded with no streams in it has no way to explain that to the user, whereas
        // a failure is something it already knows how to report.
        let _ = &session.pending_start;
        tracing::warn!("no capture path yet; failing the share");
        reply.fail(PortalResponse::Ended);
    }

    /// `Request.Close`: the application gave up while the picker was up.
    fn cancel(&mut self, request: OwnedObjectPath) {
        let pending = self
            .sessions
            .values_mut()
            .find(|session| {
                session
                    .pending_start
                    .as_ref()
                    .is_some_and(|pending| pending.request == request)
            })
            .and_then(|session| session.pending_start.take());

        match pending {
            Some(pending) => {
                tracing::info!(request = %request, "cancelled");
                pending.reply.send(PortalResponse::Canceled, Vec::new());
            }
            // Common and harmless: the call finished a moment before the cancel arrived.
            None => tracing::debug!(request = %request, "cancel for a call that already finished"),
        }
    }

    fn close_session(&mut self, path: OwnedObjectPath) {
        let Some(session) = self.sessions.remove(&path) else {
            tracing::debug!(session = %path, "close for a session that is already gone");
            return;
        };
        // A session closed while a Start is outstanding still has to answer it, or the frontend
        // waits on a reply that is never coming.
        if let Some(pending) = session.pending_start {
            pending.reply.send(PortalResponse::Canceled, Vec::new());
        }
        tracing::info!(
            session = %path,
            app_id = %session.app_id,
            count = self.sessions.len(),
            "session closed",
        );
    }
}
