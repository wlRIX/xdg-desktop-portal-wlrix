# xdg-desktop-portal-wlrix

The wlRIX desktop portal backend. Implements `org.freedesktop.impl.portal.ScreenCast`, which is what
`xdg-desktop-portal` hands an application's screen-sharing request to — so that Firefox, OBS and anything else that asks
for a screen gets one.

- **Language:** Rust
- **License:** GPL-3.0-or-later
- **Reference:** [xdg-desktop-portal-wlr](https://github.com/emersion/xdg-desktop-portal-wlr),
  [xdg-desktop-portal-cosmic](https://github.com/pop-os/xdg-desktop-portal-cosmic)

Started by D-Bus activation, not by hand and not by `wlrix-session`. On a normal wlRIX install there is nothing to set
up.

## Why not `xdg-desktop-portal-wlr`

That backend speaks `wlr-screencopy`, which can capture **outputs only**. "Share a single window" was therefore never
really possible, and it shells out to `slurp` for a region instead — which is where the window-picker failures come
from. Its `chooser_type`/`chooser_cmd`
configuration is the other half of the trouble.

`wlrix-compositor` implements `ext-image-capture-source-v1` +
`ext-image-copy-capture-v1`, which has **per-toplevel** sources. A window picker can therefore be a real window picker,
and there is nothing left to configure.

## Shape

Two processes, deliberately.

|                           |                                                                                        |
|---------------------------|----------------------------------------------------------------------------------------|
| **this**                  | Rust. D-Bus, PipeWire, and all screen capture.                                         |
| **`wlrix-source-picker`** | C#/Avalonia, from `wlrix-apps`. The dialog that asks which monitor or window to share. |

The split is not about taste. A ScreenCast portal has to produce a PipeWire stream, and PipeWire has no C# bindings —
SPA's POD builders are `static inline` in the C headers with no exported symbols, so they cannot even be reached by
P/Invoke. Rust has the PipeWire project's own bindings. Conversely the picker wants a themed dialog with a grid of live
previews, which is an afternoon in Avalonia against the `Wlrix.Avalonia` theme and a great deal longer in a hand-rolled
toolkit.

So the daemon captures everything and publishes preview frames into `$XDG_RUNTIME_DIR`; the picker reads them and draws.
The picker binds no Wayland protocols at all.

Internally one calloop loop owns all state. D-Bus runs on its own thread and reports in through a `calloop::channel` —
the shape `wlrix-idle` uses — so there is no shared mutable state and no lock between the two.

## Build

```sh
cargo build
```

Needs `libpipewire-0.3` and `libspa-0.2` development files.

## Install

```sh
cargo build --release
sudo just install
```

Five files land, and four of them exist only so the frontend can find the fifth:

| Path                                                                              |                                           |
|-----------------------------------------------------------------------------------|-------------------------------------------|
| `$PREFIX/lib/xdg-desktop-portal-wlrix`                                            | the daemon                                |
| `$PREFIX/share/xdg-desktop-portal/portals/wlrix.portal`                           | which interfaces this backend implements  |
| `$PREFIX/share/xdg-desktop-portal/wlrix-portals.conf`                             | which backend wlRIX prefers per interface |
| `$PREFIX/share/dbus-1/services/org.freedesktop.impl.portal.desktop.wlrix.service` | bus activation                            |
| `$PREFIX/lib/systemd/user/xdg-desktop-portal-wlrix.service`                       | the unit activation starts                |

**`wlrix-portals.conf` is lowercase, and that matters.** `xdg-desktop-portal` looks for
`DESKTOP-portals.conf` with the `XDG_CURRENT_DESKTOP` entry case-folded to lower case (`portals.conf(5)`), and
`wlrix-session` sets `XDG_CURRENT_DESKTOP=wlRIX`. Naming the file
`wlRIX-portals.conf` produces no error anywhere — screen sharing just silently reports no sources.

The daemon goes in `lib` rather than `bin` because it is bus-activated and never run by hand. Every other wlRIX
component is started *by name* off `$PATH`, which is why those are in `bin`.

## Checking it is there

```sh
busctl --user introspect org.freedesktop.impl.portal.desktop.wlrix /org/freedesktop/portal/desktop org.freedesktop.impl.portal.ScreenCast
```

Should show `CreateSession`, `SelectSources`, `Start`, and the `AvailableSourceTypes` /
`AvailableCursorModes` / `version` properties. If the name does not resolve, the `.service` file is missing or points at
the wrong path; if it resolves but applications still see no sources, suspect the `portals.conf` filename above.

`RUST_LOG=xdg_desktop_portal_wlrix=debug` for the details. Scope it to the crate — a bare
`RUST_LOG=debug` turns on zbus's message tracing, which buries everything worth reading.

## What is implemented

Interface version 4. Monitor and window sources; hidden and embedded cursor modes.

Not implemented, and deliberately not advertised:

- **Cursor metadata** (`AvailableCursorModes` bit 4) — needs
  `ext_image_copy_capture_cursor_session_v1`, which the compositor does not implement.
- **Virtual sources** (`AvailableSourceTypes` bit 4) — a headless output made on demand for the cast. The compositor
  cannot make one.
- **Restore tokens** (interface versions 5 and 6) — "share again without asking". Claiming the version without the
  behavior would have the frontend offer applications something that silently never works.
- **Screenshot** and **RemoteDesktop** — other backends still handle these. RemoteDesktop needs input injection the
  compositor does not have: there is no `zwlr_virtual_pointer_v1`, and virtual-keyboard is gated behind the sandbox
  check.

## Known gaps

- **The picker is not parented to the window that asked for the share.** The portal's
  `parent_window` is a `wayland:` handle for `xdg-foreign`, and Avalonia's Wayland backend implements the *export* half
  of that protocol but not the import half. The handle is logged and otherwise ignored until that lands upstream.
- **Capture is through shared memory, not dmabuf.** The compositor's
  `ext-image-copy-capture` advertises no dmabuf constraints yet, so every frame is a GPU readback plus a shm write.
  Noticeable at 1440p60. The fix belongs in
  `wlrix-compositor/src/image_capture.rs`, not here.
