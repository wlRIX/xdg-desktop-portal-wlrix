// SPDX-License-Identifier: GPL-3.0-or-later
//! PipeWire: where the captured pixels come out.
//!
//! A ScreenCast portal's whole purpose is to turn "the user picked this window" into a PipeWire
//! node an application can consume. The frontend never sees the pixels -- it takes the node ids
//! reported by `Start`, opens a PipeWire remote restricted to them, and hands that to the
//! application.
//!
//! Named `cast` rather than `pipewire` so that `pipewire::` in these files unambiguously means
//! the crate.
//!
//! ## Two loops, one thread
//!
//! PipeWire has its own event loop, and so does everything else here. Rather than run it on a
//! thread of its own -- which would put the streams and the Wayland captures that feed them on
//! opposite sides of a lock -- its loop *fd* goes on calloop, and each wakeup drives one
//! iteration. PipeWire's callbacks then run inside a calloop dispatch, on the same thread as
//! everything else, and the single-owner rule survives.
//!
//! The one consequence to remember is re-entrancy: a PipeWire callback fires while calloop is
//! already inside the event source's own handler, so it cannot take `&mut Portal`. The callbacks
//! therefore only ever record into an `Rc<RefCell<Shared>>`, which
//! [`crate::Portal::after_dispatch`] reads afterwards. Same reconcile-once-per-iteration shape
//! as the rest of this program.

pub mod stream;

use std::os::fd::AsRawFd;

use pipewire::{context::ContextRc, core::CoreRc, main_loop::MainLoopRc};

use crate::Portal;

pub use stream::{Cast, DmabufPlan, Source};

/// The PipeWire connection, kept alive for the life of the process.
///
/// The `Rc` flavours of each type are used rather than the `Box` ones because a stream has to
/// outlive the function that made it while still holding its core alive, which is exactly what
/// they are for.
pub struct PipeWire {
    pub core: CoreRc,
    _context: ContextRc,
    main_loop: MainLoopRc,
}

impl PipeWire {
    /// Drive one iteration of PipeWire's loop.
    ///
    /// `Timeout::None` -- never block. calloop has already established there is something to
    /// read, and blocking here would stall every other source on the loop.
    pub fn iterate(&self) {
        self.main_loop
            .loop_()
            .iterate(pipewire::loop_::Timeout::None);
    }
}

/// Connect to PipeWire and put its loop on calloop.
pub fn connect(handle: &calloop::LoopHandle<'static, Portal>) -> Result<PipeWire, String> {
    // Initializes the library's globals. Safe to call more than once, but it must happen before
    // anything else here.
    pipewire::init();

    let main_loop = MainLoopRc::new(None).map_err(|err| format!("PipeWire main loop: {err}"))?;
    let context =
        ContextRc::new(&main_loop, None).map_err(|err| format!("PipeWire context: {err}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|err| format!("could not connect to PipeWire: {err}"))?;

    // `iterate` both reads the fd and runs the callbacks, so calloop only has to say "there is
    // something there". Level-triggered readability is exactly what pw_loop's fd reports.
    let source = calloop::generic::Generic::new(
        // Borrowed from the loop, which lives as long as the returned `PipeWire`. Duplicated
        // rather than borrowed because calloop's source wants to own its fd.
        rustix::io::dup(main_loop.loop_().fd())
            .map_err(|err| format!("could not duplicate the PipeWire loop fd: {err}"))?,
        calloop::Interest::READ,
        calloop::Mode::Level,
    );
    handle
        .insert_source(source, |_, _, portal| {
            if let Some(pipewire) = &portal.pipewire {
                pipewire.iterate();
            }
            Ok(calloop::PostAction::Continue)
        })
        .map_err(|err| format!("could not watch the PipeWire loop: {err}"))?;

    tracing::debug!(
        fd = main_loop.loop_().fd().as_raw_fd(),
        "PipeWire connected"
    );
    Ok(PipeWire {
        core,
        _context: context,
        main_loop,
    })
}
