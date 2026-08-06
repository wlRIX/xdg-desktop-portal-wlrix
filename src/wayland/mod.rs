// SPDX-License-Identifier: GPL-3.0-or-later
//! The compositor side: what can be shared, and reading pixels out of it.
//!
//! Three protocols, and the split between them is the whole reason this backend can do what
//! `xdg-desktop-portal-wlr` cannot:
//!
//! - **`ext-foreign-toplevel-list-v1`** — the window list. Read-only: identifier, title, app id.
//! - **`ext-image-capture-source-v1`** — names *what* to capture. Two managers, one taking a
//!   `wl_output` and one taking a foreign-toplevel handle. The second is the per-window capture
//!   that `wlr-screencopy` never had.
//! - **`ext-image-copy-capture-v1`** — negotiates buffers against a source and fills them.
//!
//! All of it hangs off the same calloop the D-Bus channel does, through
//! `calloop-wayland-source`, so a compositor event and a portal call wake the same loop and
//! there is still exactly one owner of all state.
//!
//! ## Dispatch lives on `Portal`, not here
//!
//! `wayland-client` dispatches into one state type, and calloop already owns [`crate::Portal`]
//! as its loop data. Rather than keep a second state and reconcile the two, `Portal` is the
//! dispatch target and every handler below reaches into its [`Wayland`] field. That is the same
//! shape `wlrix-idle` uses.

pub mod capture;
pub mod inventory;
pub mod shm;

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
        ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
};

use crate::Portal;
use inventory::{Inventory, Monitor, Window};

/// `wl_output` version 4 or better, for the `name` and `description` events.
///
/// Not a nicety. Without `name` there is no stable identity for a monitor -- only the global's
/// numeric id, which changes when a monitor is unplugged and replugged -- so a picker selection
/// could not be matched back to the output it meant. wlRIX's compositor advertises 4.
const WL_OUTPUT_VERSION: u32 = 4;

/// The globals this backend needs, and the inventory built from them.
#[derive(Default)]
pub struct Wayland {
    /// Needed to create any protocol object, so it is kept rather than threaded through every
    /// call site. `Option` only because [`Portal`] is `Default` and a queue handle cannot be --
    /// it is set by [`connect`] before anything can use it.
    pub qh: Option<QueueHandle<Portal>>,
    pub shm: Option<WlShm>,
    pub output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub toplevel_sources: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    pub copy: Option<ExtImageCopyCaptureManagerV1>,
    pub inventory: Inventory,
    /// Capture sessions in flight, keyed by the session object.
    pub captures: Vec<capture::Capture>,
}

impl Wayland {
    /// Whether every global needed to capture anything turned up.
    ///
    /// Checked once at startup rather than at each call: a compositor that does not implement
    /// these cannot be made to later, and a portal that owns the bus name but fails every share
    /// is worse than one that refuses to start and lets another backend have the name.
    pub fn missing_globals(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.shm.is_none() {
            missing.push("wl_shm");
        }
        if self.output_sources.is_none() {
            missing.push("ext_output_image_capture_source_manager_v1");
        }
        if self.toplevel_sources.is_none() {
            missing.push("ext_foreign_toplevel_image_capture_source_manager_v1");
        }
        if self.copy.is_none() {
            missing.push("ext_image_copy_capture_manager_v1");
        }
        missing
    }

    /// Make the capture source naming a monitor.
    pub fn monitor_source(
        &self,
        qh: &QueueHandle<Portal>,
        monitor: &Monitor,
    ) -> Option<ExtImageCaptureSourceV1> {
        Some(
            self.output_sources
                .as_ref()?
                .create_source(&monitor.output, qh, ()),
        )
    }

    /// Make the capture source naming a window.
    pub fn window_source(
        &self,
        qh: &QueueHandle<Portal>,
        window: &Window,
    ) -> Option<ExtImageCaptureSourceV1> {
        Some(
            self.toplevel_sources
                .as_ref()?
                .create_source(&window.handle, qh, ()),
        )
    }
}

/// Connect, bind the globals, and put the connection on the loop.
///
/// Returns once the inventory has settled, so the first `SelectSources` does not race the window
/// list: the toplevel list is only bound during the first roundtrip, its window announcements
/// arrive during the second, and their titles during the third. Three rather than a loop with a
/// timeout because the sequence is exactly this long -- the reference client in
/// `wlrix-compositor/examples/test_image_capture.rs` does the same and says so.
pub fn connect(
    handle: &calloop::LoopHandle<'static, Portal>,
    portal: &mut Portal,
) -> Result<Connection, String> {
    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no compositor to connect to: {err}"))?;
    let mut queue = connection.new_event_queue::<Portal>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    portal.wayland.qh = Some(qh);

    for stage in ["bind the globals", "list the windows", "read their titles"] {
        queue
            .roundtrip(portal)
            .map_err(|err| format!("could not {stage}: {err}"))?;
    }

    let missing = portal.wayland.missing_globals();
    if !missing.is_empty() {
        return Err(format!(
            "the compositor does not implement {} -- screen sharing is not possible",
            missing.join(", ")
        ));
    }

    calloop_wayland_source::WaylandSource::new(connection.clone(), queue)
        .insert(handle.clone())
        .map_err(|err| format!("could not watch the Wayland connection: {err}"))?;

    Ok(connection)
}

impl Dispatch<WlRegistry, ()> for Portal {
    fn event(
        portal: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => portal.wayland.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    // Every output, not just the first: the picker offers all of them, and this
                    // machine runs two. Bound at 4 for `name`; a compositor offering less is not
                    // one this backend can identify outputs on.
                    if version < WL_OUTPUT_VERSION {
                        tracing::warn!(
                            version,
                            "wl_output below version {WL_OUTPUT_VERSION}; monitors cannot be \
                             identified and will not be offered",
                        );
                        return;
                    }
                    let output: WlOutput = registry.bind(name, WL_OUTPUT_VERSION, qh, ());
                    portal.wayland.inventory.monitors.push(Monitor {
                        output,
                        name: String::new(),
                        description: String::new(),
                        position: (0, 0),
                        size: (0, 0),
                        ready: false,
                    });
                }
                "ext_output_image_capture_source_manager_v1" => {
                    portal.wayland.output_sources = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    portal.wayland.toplevel_sources = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    portal.wayland.copy = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_foreign_toplevel_list_v1" => {
                    // Bound for its events alone -- the handle is never needed again, since a
                    // window is reached through the handles this hands out.
                    let _list: ExtForeignToplevelListV1 = registry.bind(name, 1, qh, ());
                }
                _ => {}
            },
            // A monitor unplugged. The `wl_output` is already dead, so anything capturing it
            // will be stopped by the compositor; this only takes it out of the picker.
            wl_registry::Event::GlobalRemove { name } => {
                let before = portal.wayland.inventory.monitors.len();
                portal
                    .wayland
                    .inventory
                    .monitors
                    .retain(|monitor| monitor.output.id().protocol_id() != name);
                if portal.wayland.inventory.monitors.len() != before {
                    tracing::info!(global = name, "a monitor went away");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for Portal {
    fn event(
        portal: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(monitor) = portal.wayland.inventory.monitor_mut(output) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => monitor.name = name,
            wl_output::Event::Description { description } => monitor.description = description,
            wl_output::Event::Geometry { x, y, .. } => monitor.position = (x, y),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Several modes are announced; only the current one is the size to capture at.
                if flags
                    .into_result()
                    .is_ok_and(|flags| flags.contains(wl_output::Mode::Current))
                {
                    monitor.size = (width, height);
                }
            }
            // Everything since the last `done` is one consistent description.
            wl_output::Event::Done => {
                monitor.ready = true;
                tracing::debug!(
                    name = %monitor.name,
                    size = ?monitor.size,
                    position = ?monitor.position,
                    "monitor",
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Portal {
    fn event(
        portal: &mut Self,
        _list: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            portal.wayland.inventory.windows.push(Window {
                handle: toplevel,
                identifier: String::new(),
                title: String::new(),
                app_id: String::new(),
                ready: false,
            });
        }
    }

    // The `toplevel` event carries a new object, so the queue has to be told what to make of it.
    wayland_client::event_created_child!(Portal, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Portal {
    fn event(
        portal: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `Closed` removes the window rather than updating it, so it is handled before the
        // lookup that every other event needs.
        if matches!(event, ext_foreign_toplevel_handle_v1::Event::Closed) {
            if let Some(window) = portal.wayland.inventory.window_mut(handle) {
                tracing::info!(title = %window.label(), "a window went away");
            }
            portal
                .wayland
                .inventory
                .windows
                .retain(|window| &window.handle != handle);
            return;
        }

        let Some(window) = portal.wayland.inventory.window_mut(handle) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                window.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => window.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => window.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Done => {
                // A window with no identifier cannot be picked -- there would be nothing to send
                // back -- so it stays out of the inventory rather than being offered and failing.
                let was_ready = window.ready;
                window.ready = !window.identifier.is_empty();
                if !window.ready {
                    tracing::warn!("a window announced no identifier; not offering it");
                } else if !was_ready {
                    // Only the first `done`: a title change re-sends one, and logging every
                    // rename would bury the arrivals among them.
                    tracing::info!(title = %window.label(), "a window appeared");
                }
            }
            _ => {}
        }
    }
}

/// Objects this backend drives but reads no events from.
///
/// The managers are pure factories, and `wl_shm_pool`/`wl_buffer`/the source objects have no
/// events worth acting on here -- a `wl_buffer.release` matters to a client that reuses buffers
/// on its own schedule, and this one is told when a frame is done by the capture protocol.
macro_rules! ignore_events {
    ($($ty:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for Portal {
            fn event(
                _portal: &mut Self,
                _obj: &$ty,
                _event: <$ty as Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
ignore_events!(
    WlShm,
    WlShmPool,
    WlBuffer,
    ExtImageCaptureSourceV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtImageCopyCaptureManagerV1,
);
