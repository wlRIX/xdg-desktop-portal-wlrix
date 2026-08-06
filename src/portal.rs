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

use crate::{
    config::Config,
    dbus::{PortalResponse, Reply, Request, SelectOptions, Stream},
    wayland::capture::{Capture, Outcome, Purpose},
};

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
    /// What this session is sharing, as inventory ids. Written when the share begins; the link
    /// from a session to its casts and captures, which are keyed the same way.
    pub sources: Vec<String>,
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
    /// The compositor side: what can be shared, and the captures reading from it.
    ///
    /// A field rather than a separate loop state because `wayland-client` dispatches into one
    /// type and calloop already owns this one. Every `Dispatch` impl in [`crate::wayland`] is
    /// written for `Portal` and reaches in here.
    pub wayland: crate::wayland::Wayland,
    /// The Wayland connection, kept for its `flush` -- see [`Portal::after_dispatch`].
    pub connection: Option<wayland_client::Connection>,
    /// The loop, so the picker's stdout can be watched when one is spawned. Sources cannot be
    /// registered up front here: the fd does not exist until there is a picker.
    pub handle: Option<calloop::LoopHandle<'static, Portal>>,
    /// The bus connection, for the one thing that is not a reply: `Session.Closed`.
    pub bus: Option<zbus::blocking::Connection>,
    /// The PipeWire connection. `None` until [`crate::cast::connect`] runs, and `None` for
    /// `--probe`, which never publishes anything.
    pub pipewire: Option<crate::cast::PipeWire>,
    /// The live screen shares. One per source the user picked, each a PipeWire node.
    pub casts: Vec<crate::cast::Cast>,
    /// Live thumbnails, while a picker is open.
    pub previews: Option<crate::preview::Previews>,
    /// The picker, while one is waiting on the user. At most one: the dialog is modal in
    /// spirit, and two at once would be two questions about the same screen.
    pub picker: Option<crate::picker::Picker>,
    /// What the open picker was asked, kept so its answer can be checked against it.
    picking: Option<Picking>,
    pub config: Config,
    /// Captures opened for a `Start` whose stream cannot exist yet.
    ///
    /// A PipeWire stream's format is the size the compositor will produce, and the compositor
    /// only says what that is after the capture session is opened. So the two are created a beat
    /// apart, and this is the beat.
    starting: Vec<Starting>,
}

/// A session's object path as a directory name.
///
/// The path is `/org/freedesktop/portal/desktop/session/<token>/<id>` -- unique already, but
/// full of slashes. Reduced to its last two segments joined by an underscore, which keeps it
/// unique between concurrent sessions without nesting directories per component.
fn session_key(path: &OwnedObjectPath) -> String {
    let mut last: Vec<&str> = path.as_str().rsplit('/').take(2).collect();
    last.reverse();
    last.join("_")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The question a running picker was asked.
struct Picking {
    session: OwnedObjectPath,
    options: SelectOptions,
    /// Exactly what was offered, which is the only thing its answer may name.
    candidates: Vec<(String, bool)>,
}

/// A share waiting on the compositor to say how big its frames will be.
///
/// A [`crate::cast::Source`] and nothing else: everything needed to build the stream is settled
/// at `Start`, and only the frame size is still to come.
type Starting = crate::cast::Source;

impl Portal {
    /// Collect anything the compositor finished during this loop iteration.
    ///
    /// Frames are collected here rather than inside the protocol handlers because a handler
    /// cannot reach the buffer a frame belongs to while the event queue borrows the state --
    /// and because doing it once per iteration is how the compositor drives the other side of
    /// these same protocols.
    pub fn after_dispatch(&mut self) {
        self.pump_previews();
        self.create_ready_streams();
        self.pump_frames();
        self.answer_ready_starts();
        self.reap_stopped();

        // Flush, or none of the above reaches the compositor.
        //
        // This runs *after* the Wayland source has already dispatched and flushed for this
        // iteration, so every request made above -- crucially the next frame's `capture` -- is
        // still sitting in the connection's outgoing buffer. Without this the stream delivers
        // exactly one frame and then hangs: the compositor is never told to produce another, so
        // it never replies, so nothing wakes the loop with anything to do.
        if let Some(connection) = &self.connection
            && let Err(err) = connection.flush()
        {
            tracing::warn!("could not flush the Wayland connection: {err}");
        }
    }

    /// Put everything down before the process exits.
    ///
    /// Dropping the collections is what does the work: a `Cast` disconnects its stream, a
    /// `Capture` destroys its session, a `Picker` kills the dialog, and `Previews` removes its
    /// files. Doing it here rather than leaving it to process teardown means applications see
    /// their nodes disappear -- and, more visibly, that no picker is left on screen asking
    /// about a share that nothing is listening for any more.
    pub fn shut_down(&mut self) {
        let sessions: Vec<OwnedObjectPath> = self.sessions.keys().cloned().collect();
        for path in sessions {
            // Tell each application its share is over rather than just vanishing.
            self.end_session(&path);
        }
        self.picker = None;
        if let Some(previews) = self.previews.take() {
            previews.close(&mut self.wayland);
        }
        self.casts.clear();
        self.wayland.captures.clear();
        // The compositor will not see the destroys otherwise: this runs after the loop's last
        // dispatch, so nothing else is going to flush.
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
    }

    /// Keep the picker's thumbnails moving.
    fn pump_previews(&mut self) {
        let (Some(previews), Some(qh)) = (&mut self.previews, self.wayland.qh.clone()) else {
            return;
        };
        previews.pump(&mut self.wayland, &qh);
    }

    /// Turn captures whose constraints have arrived into PipeWire streams.
    fn create_ready_streams(&mut self) {
        let Some(pipewire) = &self.pipewire else {
            return;
        };
        let mut still_starting = Vec::new();
        for starting in std::mem::take(&mut self.starting) {
            let Some(capture) = self
                .wayland
                .captures
                .iter()
                .find(|capture| capture.source_id == starting.id)
            else {
                // The capture is gone: the source went away between `Start` and now.
                continue;
            };
            let Some(constraints) = capture.constraints else {
                still_starting.push(starting);
                continue;
            };
            let (Some(shm), Some(qh)) = (&self.wayland.shm, &self.wayland.qh) else {
                continue;
            };
            let source_id = starting.id.clone();
            match crate::cast::Cast::new(&pipewire.core, shm, qh, starting, constraints) {
                Ok(cast) => {
                    // The stream was built *from* these constraints, so the pending
                    // "reconfigured" they arrived with is answered, not a resize. Clearing it
                    // here is what keeps `pump_frames` from reporting a size change on every
                    // share the moment it starts.
                    if let Some(capture) = self
                        .wayland
                        .captures
                        .iter_mut()
                        .find(|capture| capture.source_id == source_id)
                    {
                        capture.take_reconfigure();
                    }
                    self.casts.push(cast);
                }
                Err(err) => tracing::error!(source = %source_id, "{err}"),
            }
        }
        self.starting = still_starting;
    }

    /// Move frames from the compositor to PipeWire.
    ///
    /// The two halves are deliberately independent: the capture runs at whatever rate the
    /// compositor repaints, and frames are handed on whenever PipeWire has a buffer free.
    /// Coupling them -- capturing straight into a PipeWire buffer -- deadlocks; see the
    /// `crate::cast::stream` module docs.
    fn pump_frames(&mut self) {
        let (Some(qh), Some(shm)) = (self.wayland.qh.clone(), self.wayland.shm.clone()) else {
            return;
        };

        for index in 0..self.casts.len() {
            let source_id = self.casts[index].source.id.clone();
            let Some(capture) = self
                .wayland
                .captures
                .iter()
                .position(|capture| capture.source_id == source_id)
            else {
                continue;
            };

            // The source changed size. The stream follows it rather than stopping, which keeps
            // the node id the application was given -- see `Cast::reconfigure`.
            if self.wayland.captures[capture].take_reconfigure()
                && let Some(constraints) = self.wayland.captures[capture].constraints
                && let Err(err) = self.casts[index].reconfigure(&shm, &qh, constraints)
            {
                tracing::error!(source = %source_id, "{err}");
            }

            match self.wayland.captures[capture].take_outcome() {
                Some(Outcome::Ready) => self.casts[index].frame_captured(),
                Some(Outcome::Failed(reason)) => {
                    tracing::debug!(source = %source_id, "frame failed: {reason}")
                }
                None => {}
            }

            // Hand the newest frame on if PipeWire has somewhere to put it.
            self.casts[index].submit();

            // Then keep the capture busy, whatever PipeWire is doing. Disjoint fields, so both
            // can be borrowed at once: the buffer belongs to `casts`, the capture to `wayland`.
            if !self.wayland.captures[capture].busy() {
                let buffer = self.casts[index].capture_buffer();
                self.wayland.captures[capture].request(&qh, buffer);
            }
        }
    }

    /// Answer any `Start` whose streams now have node ids.
    fn answer_ready_starts(&mut self) {
        let ready: Vec<OwnedObjectPath> = self
            .sessions
            .iter()
            .filter(|(path, session)| {
                session.pending_start.is_some() && self.streams_for(path).is_some()
            })
            .map(|(path, _)| path.clone())
            .collect();

        for path in ready {
            let Some(streams) = self.streams_for(&path) else {
                continue;
            };
            let Some(pending) = self
                .sessions
                .get_mut(&path)
                .and_then(|session| session.pending_start.take())
            else {
                continue;
            };
            tracing::info!(session = %path, streams = streams.len(), "sharing");
            pending.reply.send(PortalResponse::Success, streams);
        }
    }

    /// The stream list for a session, or `None` while any of its streams is still coming up.
    ///
    /// All or nothing: the interface says `Start` reports every stream at once, so a partial
    /// list would have the application open a remote missing one of the sources it asked for.
    fn streams_for(&self, session: &OwnedObjectPath) -> Option<Vec<Stream>> {
        let casts: Vec<&crate::cast::Cast> = self
            .casts
            .iter()
            .filter(|cast| self.session_owns(session, &cast.source.id))
            .collect();
        if casts.is_empty() {
            return None;
        }
        casts
            .iter()
            .map(|cast| cast.node_id().map(|id| (id, cast.stream_properties())))
            .collect()
    }

    /// Whether this session is the one sharing that source.
    fn session_owns(&self, session: &OwnedObjectPath, source_id: &str) -> bool {
        self.sessions
            .get(session)
            .is_some_and(|session| session.sources.iter().any(|id| id == source_id))
    }

    /// End a session this backend decided is over, and tell the application so.
    ///
    /// The other direction from [`Portal::close_session`], and the two must not be confused.
    /// There, the frontend said stop and already knows; here, the source went away and the
    /// application has no idea -- so `Closed` is emitted, which is the only way it learns that
    /// the black rectangle it is showing its users is never going to move again.
    fn end_session(&mut self, path: &OwnedObjectPath) {
        let Some(session) = self.sessions.remove(path) else {
            return;
        };
        if let Some(pending) = session.pending_start {
            pending.reply.send(PortalResponse::Ended, Vec::new());
        }
        self.stop_sources(&session.sources);

        let Some(bus) = &self.bus else {
            return;
        };
        // No details. The signal's argument is a vardict for future use and nothing is defined
        // to put in it; an empty one is what every other backend sends.
        let details: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        if let Err(err) = bus.emit_signal(
            None::<&str>,
            path,
            "org.freedesktop.impl.portal.Session",
            "Closed",
            &(details,),
        ) {
            tracing::warn!(session = %path, "could not tell the application the session ended: {err}");
        }
        tracing::info!(session = %path, app_id = %session.app_id, "session ended by the backend");
    }

    /// Drop what the compositor has stopped, and end the streams that fed on it.
    fn reap_stopped(&mut self) {
        let stopped: Vec<String> = self
            .wayland
            .captures
            .iter()
            .filter(|capture| capture.stopped)
            .map(|capture| capture.source_id.clone())
            .collect();
        if stopped.is_empty() {
            return;
        }
        self.wayland.captures.retain(|capture| !capture.stopped);
        for source_id in &stopped {
            // Dropping the cast disconnects the stream, which is how the application learns
            // *that* stream is over -- its node disappears from the graph.
            self.casts.retain(|cast| &cast.source.id != source_id);
            tracing::info!(source = %source_id, "the shared source went away; stream ended");
        }

        // A session whose last stream has gone is over, and the application has to be told.
        //
        // Only the *last* one: a session sharing two monitors, one of which is unplugged, still
        // has a live stream and is still a session. Closing it there would take away the screen
        // the user is still sharing because a different one went away.
        let finished: Vec<OwnedObjectPath> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                !session.sources.is_empty()
                    && session.pending_start.is_none()
                    && !session
                        .sources
                        .iter()
                        .any(|id| self.casts.iter().any(|cast| &cast.source.id == id))
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in finished {
            self.end_session(&path);
        }
    }

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
                sources: Vec::new(),
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

        // Everything the request could be answered with, which is both what the picker shows
        // and what its answer is checked against.
        let candidates = self.candidates(&options);
        if candidates.is_empty() {
            tracing::warn!("nothing to share matches what was asked for");
            reply.fail(PortalResponse::Ended);
            return;
        }

        let previews =
            match crate::preview::Previews::new(&session_key(&path), &candidates, &self.config) {
                Ok(previews) => Some(previews),
                Err(err) => {
                    // The picker can still show a grid of labels, so this is not fatal.
                    tracing::warn!("no live previews: {err}");
                    None
                }
            };
        if let (Some(previews), Some(qh)) = (&previews, self.wayland.qh.clone()) {
            previews.open_captures(&mut self.wayland, &qh);
        }

        let manifest = self.manifest(&app_id, &options, &candidates, previews.as_ref());
        let mut picker = match crate::picker::Picker::spawn(&manifest, request.clone()) {
            Ok(picker) => picker,
            Err(err) => {
                tracing::error!("{err}");
                if let Some(previews) = previews {
                    previews.close(&mut self.wayland);
                }
                reply.fail(PortalResponse::Ended);
                return;
            }
        };

        self.previews = previews;
        if let Err(err) = self.watch_picker(&mut picker) {
            tracing::error!("{err}");
            if let Some(previews) = self.previews.take() {
                previews.close(&mut self.wayland);
            }
            picker.cancel();
            reply.fail(PortalResponse::Ended);
            return;
        }
        self.picker = Some(picker);
        self.picking = Some(Picking {
            session: path.clone(),
            options,
            candidates,
        });

        // Answered from `after_dispatch`, once the user has chosen and PipeWire has assigned
        // the node ids. Until then the call is genuinely outstanding, which is what
        // `pending_start` is for -- and what makes `Request.Close` meaningful.
        let Some(session) = self.sessions.get_mut(&path) else {
            return;
        };
        session.pending_start = Some(PendingStart { request, reply });
    }

    /// Watch the picker's stdout, so its answer arrives as an event like anything else.
    ///
    /// Non-blocking, because calloop reporting the fd readable does not promise a whole line is
    /// there -- a blocking read on a partial answer would stop the loop, and with it the
    /// thumbnails the user is looking at while deciding.
    fn watch_picker(&mut self, picker: &mut crate::picker::Picker) -> Result<(), String> {
        let handle = self
            .handle
            .clone()
            .ok_or("no event loop to watch the picker")?;
        let stdout = picker.take_stdout().ok_or("the picker has no stdout")?;
        let flags = rustix::fs::fcntl_getfl(&stdout)
            .map_err(|err| format!("could not read the picker's stdout flags: {err}"))?;
        rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|err| format!("could not make the picker's stdout non-blocking: {err}"))?;

        handle
            .insert_source(
                calloop::generic::Generic::new(
                    stdout,
                    calloop::Interest::READ,
                    calloop::Mode::Level,
                ),
                |_, stdout, portal: &mut Portal| {
                    let Some(picker) = &mut portal.picker else {
                        return Ok(calloop::PostAction::Remove);
                    };
                    if !picker.read_available(&**stdout) {
                        return Ok(calloop::PostAction::Continue);
                    }
                    // stdout closed: the picker is done talking.
                    if let Some(picker) = portal.picker.take() {
                        portal.picked(picker.finish());
                    }
                    Ok(calloop::PostAction::Remove)
                },
            )
            .map_err(|err| format!("could not watch the picker: {err}"))?;
        Ok(())
    }

    /// Everything that could answer this request, as (id, is_monitor).
    ///
    /// Monitors first, because that is the order the picker shows them and a request allowing
    /// both usually means "the screen".
    fn candidates(&self, options: &SelectOptions) -> Vec<(String, bool)> {
        const MONITOR: u32 = 1;
        const WINDOW: u32 = 2;
        let mut candidates = Vec::new();
        if options.types & MONITOR != 0 {
            candidates.extend(
                self.wayland
                    .inventory
                    .ready_monitors()
                    .map(|monitor| (monitor.name.clone(), true)),
            );
        }
        if options.types & WINDOW != 0 {
            candidates.extend(
                self.wayland
                    .inventory
                    .ready_windows()
                    // Never the picker itself. Its window is not normally there yet -- the list
                    // is built before it is spawned -- but one left over from a previous
                    // question would be, and "share the dialog asking what to share" is not an
                    // answer anybody wants.
                    .filter(|window| window.app_id != crate::picker::APP_ID)
                    .map(|window| (window.identifier.clone(), false)),
            );
        }
        candidates
    }

    /// Describe the candidates for the picker.
    fn manifest(
        &self,
        app_id: &str,
        options: &SelectOptions,
        candidates: &[(String, bool)],
        previews: Option<&crate::preview::Previews>,
    ) -> crate::picker::Manifest {
        const CURSOR_MODE_EMBEDDED: u32 = 2;
        let sources = candidates
            .iter()
            .filter_map(|(id, is_monitor)| {
                let preview = previews
                    .and_then(|previews| previews.path_for(id))
                    .map(|path| path.display().to_string());
                let source = if *is_monitor {
                    let monitor = self.wayland.inventory.monitor(id)?;
                    crate::picker::Source {
                        id: id.clone(),
                        kind: "monitor",
                        // The description is the make and model; the name is the connector.
                        // Both are shown, because two identical monitors are told apart only by
                        // which connector they are on.
                        label: if monitor.description.is_empty() {
                            monitor.name.clone()
                        } else {
                            format!("{} ({})", monitor.description, monitor.name)
                        },
                        app_id: String::new(),
                        width: monitor.size.0.max(0) as u32,
                        height: monitor.size.1.max(0) as u32,
                        preview,
                    }
                } else {
                    let window = self.wayland.inventory.window(id)?;
                    crate::picker::Source {
                        id: id.clone(),
                        kind: "window",
                        label: window.label().to_string(),
                        app_id: window.app_id.clone(),
                        // The compositor reports a window's capture size only once a session is
                        // open on it, so zero here means "not known yet" and the picker sizes
                        // the tile from the preview instead.
                        width: 0,
                        height: 0,
                        preview,
                    }
                };
                Some(source)
            })
            .collect();

        crate::picker::Manifest {
            app_id: app_id.to_string(),
            multiple: options.multiple,
            cursor: options.cursor_mode == CURSOR_MODE_EMBEDDED,
            sources,
        }
    }

    /// Act on what the user chose.
    fn picked(&mut self, outcome: crate::picker::Outcome) {
        if let Some(previews) = self.previews.take() {
            previews.close(&mut self.wayland);
        }
        let Some(picking) = self.picking.take() else {
            return;
        };

        let selection = match outcome {
            crate::picker::Outcome::Accepted(selection) => selection,
            crate::picker::Outcome::Canceled => {
                tracing::info!("the share was canceled");
                self.answer_start(&picking.session, PortalResponse::Canceled);
                return;
            }
            crate::picker::Outcome::Failed(err) => {
                tracing::error!("{err}");
                self.answer_start(&picking.session, PortalResponse::Ended);
                return;
            }
        };

        // Only what was offered. The picker is a separate process and its answer is input like
        // any other: an id that was not in the manifest names a source this request was never
        // allowed to see.
        let chosen: Vec<(String, bool)> = selection
            .sources
            .iter()
            .filter_map(|id| {
                picking
                    .candidates
                    .iter()
                    .find(|(candidate, _)| candidate == id)
                    .cloned()
            })
            .collect();
        if chosen.len() != selection.sources.len() {
            tracing::warn!("the picker returned a source that was not offered; ignoring it");
        }
        // `multiple` was false, so honor it whatever came back.
        let chosen = if picking.options.multiple {
            chosen
        } else {
            chosen.into_iter().take(1).collect()
        };
        if chosen.is_empty() {
            self.answer_start(&picking.session, PortalResponse::Canceled);
            return;
        }

        for (source_id, is_monitor) in &chosen {
            if let Err(err) =
                self.begin_cast(&picking.session, source_id, *is_monitor, &picking.options)
            {
                tracing::error!(source = %source_id, "could not start the share: {err}");
            }
        }
        if self.starting.is_empty() {
            self.answer_start(&picking.session, PortalResponse::Ended);
        }
    }

    /// Answer an outstanding `Start` that is not going to produce streams.
    fn answer_start(&mut self, session: &OwnedObjectPath, response: PortalResponse) {
        if let Some(pending) = self
            .sessions
            .get_mut(session)
            .and_then(|session| session.pending_start.take())
        {
            pending.reply.send(response, Vec::new());
        }
    }

    /// Open the capture and the PipeWire stream for one source.
    fn begin_cast(
        &mut self,
        session_path: &OwnedObjectPath,
        source_id: &str,
        is_monitor: bool,
        options: &SelectOptions,
    ) -> Result<(), String> {
        const CURSOR_MODE_EMBEDDED: u32 = 2;
        let with_cursor = options.cursor_mode == CURSOR_MODE_EMBEDDED;

        let qh = self.wayland.qh.clone().ok_or("no Wayland queue")?;
        let copy = self
            .wayland
            .copy
            .clone()
            .ok_or("no ext_image_copy_capture_manager_v1")?;

        let (source, label, position) = if is_monitor {
            let monitor = self
                .wayland
                .inventory
                .monitor(source_id)
                .ok_or("that monitor is gone")?;
            let (label, position) = (monitor.name.clone(), monitor.position);
            (
                self.wayland
                    .monitor_source(&qh, monitor)
                    .ok_or("no output capture source manager")?,
                label,
                position,
            )
        } else {
            let window = self
                .wayland
                .inventory
                .window(source_id)
                .ok_or("that window is gone")?;
            let label = window.label().to_string();
            (
                self.wayland
                    .window_source(&qh, window)
                    .ok_or("no toplevel capture source manager")?,
                label,
                (0, 0),
            )
        };

        let capture = Capture::new(
            &copy,
            &qh,
            source,
            source_id.to_string(),
            Purpose::Cast,
            with_cursor,
        );
        self.wayland.captures.push(capture);

        // The stream cannot be created yet: its format is the size the compositor is about to
        // announce, and that arrives asynchronously. `after_dispatch` creates it once the
        // constraints land.
        self.starting.push(Starting {
            id: source_id.to_string(),
            label,
            is_monitor,
            position,
        });
        if let Some(session) = self.sessions.get_mut(session_path) {
            session.sources.push(source_id.to_string());
        }
        Ok(())
    }

    /// `Request.Close`: the application gave up while the picker was up.
    fn cancel(&mut self, request: OwnedObjectPath) {
        // Take the dialog off the screen. Leaving it up would ask the user to choose a source
        // for a share that has already been abandoned, and the answer would go nowhere.
        if let Some(picker) = &mut self.picker
            && picker.request == request
        {
            picker.cancel();
        }

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
                tracing::info!(request = %request, "canceled");
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

        // And the session's own picker, if it was still asking.
        if self
            .picking
            .as_ref()
            .is_some_and(|picking| picking.session == path)
        {
            self.picking = None;
            if let Some(previews) = self.previews.take() {
                previews.close(&mut self.wayland);
            }
            self.picker = None;
        }

        // The whole point of this call. Without it the compositor goes on capturing and the
        // PipeWire node stays published for a share that ended -- which for a browser tab
        // closed mid-call means the green "sharing" light never goes out.
        let stopped = self.stop_sources(&session.sources);

        tracing::info!(
            session = %path,
            app_id = %session.app_id,
            streams = stopped,
            count = self.sessions.len(),
            "session closed",
        );
    }

    /// Tear down everything keyed to these sources, returning how many streams ended.
    ///
    /// Only the *cast* captures. A preview capture may be running on the same source for a
    /// picker that is still open, and belongs to that picker rather than to this session.
    fn stop_sources(&mut self, sources: &[String]) -> usize {
        let before = self.casts.len();
        for id in sources {
            self.casts.retain(|cast| &cast.source.id != id);
            self.wayland
                .captures
                .retain(|capture| capture.purpose != Purpose::Cast || &capture.source_id != id);
            // A share that never got as far as a stream still has a capture waiting on
            // constraints; it has to go too, or `create_ready_streams` will build a stream for
            // a session that no longer exists.
            self.starting.retain(|starting| &starting.id != id);
        }
        before - self.casts.len()
    }
}
