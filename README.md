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

Needs `libpipewire-0.3`, `libspa-0.2` and `libgbm` development files. (On Debian and Ubuntu:
`libpipewire-0.3-dev libspa-0.2-dev libgbm-dev`.) Missing any of them shows up only as a link
error from `cargo build` — `cargo clippy` never links, so it passes regardless.

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

## The picker protocol

`wlrix-source-picker` is spawned from `PATH` for one question and exits with the answer. This is the whole contract
between the two halves; the picker binds no Wayland protocols and links no PipeWire.

**In** — a JSON manifest on stdin, closed straight after:

```json
{
  "app_id": "org.mozilla.firefox",
  "multiple": false,
  "cursor": true,
  "sources": [
    {
      "id": "DisplayPort-4",
      "kind": "monitor",
      "label": "DELL U2515H (DisplayPort-4)",
      "app_id": "",
      "width": 2560,
      "height": 1440,
      "preview": "/run/user/1000/wlrix-portal/session_a1/preview-DisplayPort-4.raw"
    },
    {
      "id": "oGhQDAH09yq7",
      "kind": "window",
      "label": "Inbox — Thunderbird",
      "app_id": "thunderbird",
      "width": 0,
      "height": 0,
      "preview": "/run/user/1000/wlrix-portal/session_a1/preview-oGhQDAH09yq7.raw"
    }
  ]
}
```

`width`/`height` are `0` for a window — the compositor only reports a window's capture size once a session is open on
it, so size the tile from the preview instead. `preview` may be absent; show the tile without one rather than dropping
the source.

**Out** — on stdout, when the user accepts:

```json
{
  "sources": [
    "DisplayPort-4"
  ]
}
```

**Exit code** — `0` accepted, `1` cancelled, anything else failed. Both signals are checked: a picker that dies
mid-answer produces neither valid stdout nor a zero exit. An empty `sources` with exit 0 counts as canceled. Ids not in
the manifest are discarded, and more ids than `multiple` allows are truncated.

stderr is the picker's log and joins this program's in the journal.

### Preview files

Each is a fixed-size file, rewritten in place, that the picker maps once and watches:

| Offset |       |                                                                  |
|--------|-------|------------------------------------------------------------------|
| 0      | `u32` | magic, `0x58524C57` (`WLRX`)                                     |
| 4      | `u32` | version, currently 1                                             |
| 8, 12  | `u32` | width, height of the thumbnail                                   |
| 16     | `u32` | stride in bytes                                                  |
| 20     | `u32` | format; `0` = BGRA/BGRX 8888                                     |
| 24     | `u32` | sequence                                                         |
| 28     | `u32` | the source's own size, `width << 16 \| height`, for letterboxing |
| 32     |       | pixels                                                           |

**Check the magic and version before trusting the dimensions** — the directory name is predictable, and a reader that
maps whatever it finds and believes the header can be made to read out of bounds.

The sequence number is a seqlock: **odd means a write is in progress**. Read it, read the pixels, read it again; if it
changed or was odd, the frame is torn — try again. It starts at 0, so a tile that has never been published reads even
with no pixels.

Sources take turns being captured (the protocol has no server-side downscale, so every capture arrives at full size), so
with several sources each tile refreshes roughly once a second rather than at video rate. `tick_ms` and `tile` in
`portal.toml` tune that.

## Configuration

`$XDG_CONFIG_HOME/wlrix/portal.toml`, else `/etc/wlrix/portal.toml`. There is no file by default and the defaults are
what everything was tuned with. Unknown keys are an error, as elsewhere in wlRIX.

```toml
[preview]
tick_ms = 100     # how often *one* source is captured; they take turns
tile = [320, 180] # thumbnail size

[capture]
dmabuf = false    # offer GPU memory as well as shared memory — see below
```

`capture.dmabuf` is safe to switch on: the stream offers shared memory alongside the dmabuf, so a consumer that cannot
import one falls back instead of failing. That is verified — GStreamer declines the dmabuf and streams over shm without
a hiccup.

It is off by default anyway, because nothing has yet *accepted* one. The compositor renders into a dmabuf happily and
the stream negotiates a real DRM modifier, but no consumer available here negotiates it, so the last stretch is untested
against a real application. Turning it on is how that gets tested.

It does not decide whether frames are copied. They are not, either way: both paths render into the very memory the
consumer reads.

## While a share is running

- **A source that changes size is followed**, not dropped. Maximizing a shared window renegotiates the PipeWire format
  in place, which keeps the node id — the application was told that id by `Start` and has no way to be told a new one,
  so restarting the stream would simply stop it.
- **A source that goes away ends the share.** The compositor stops the capture, the stream is disconnected, and
  `Session.Closed` tells the application — the only way it learns that the picture it is showing will never move again.
  A session sharing several sources ends only when the last one is gone.
- **`Session.Close`, and stopping the service, tear everything down**: captures released, streams disconnected, any open
  picker killed, preview files removed.

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

## Status

Working end to end. Verified on real hardware with **OBS** under a wlRIX session on the KVM: monitor and window sources,
the picker with live previews, and a stream that survives the shared window being resized.

## Follow-ups, in the order they are worth doing

1. **dmabuf capture — built, negotiated, and still never accepted by anything.** Every piece is in place.
   `wayland/dmabuf.rs` opens the render node the compositor names (by `dev_t`, resolved against `/dev/dri`), allocates
   through gbm with the offered modifiers, and hands the compositor a `wl_buffer` via
   `zwp_linux_dmabuf_v1`; `--probe` captures a monitor and a window straight into one and the compositor answers
   `ready`. The cast offers it as `SPA_DATA_DmaBuf` with one `spa_data` block per plane and the DRM modifier announced
   as a single **mandatory** property — pinned by test-allocating first, which sidesteps the `DONT_FIXATE` round trip
   entirely. Verified against an AMD card: modifier `0x200000028a6bf04`, two planes.

   What is left is a consumer that says yes. GStreamer, the only one testable from a terminal here, takes the shm format
   on offer — correctly, and that fallback is itself worth having proven, but it means the dmabuf path has never carried
   a live frame through PipeWire. **Switch `capture.dmabuf` on and share to OBS**; that is the whole remaining test, and
   the answer decides whether it becomes the default.

   The shm path stays regardless: a client, or a machine, that cannot allocate on the render node still has to work.
2. **Embedded cursor mode, unverified.** `cursor_mode=EMBEDDED` is implemented (`PaintCursors` on the capture session)
   but has never been *seen* working — confirming it needs the pointer physically over the captured area, which no
   automated test here could arrange. Worth one deliberate look during daily use.
3. **Restore tokens** (`persist_mode`, interface versions 5 and 6), so a browser can re-share without asking again. The
   most visible remaining papercut for anyone actually using this every day.
4. **A live source list while the picker is open.** Windows opening or closing mid-question are not reflected; the
   manifest is a snapshot taken at `Start`. Would make the stdin manifest a line-delimited stream.
5. **`org.freedesktop.impl.portal.Screenshot`.** Same capture path, no PipeWire, plus a region-select overlay. Cheap now
   that everything underneath it exists.
6. **RemoteDesktop** — blocked on the compositor: there is no `zwlr_virtual_pointer_v1`, and virtual-keyboard is behind
   the sandbox check.

## Known gaps

- **The picker is not parented to the window that asked for the share.** The portal's
  `parent_window` is a `wayland:` handle for `xdg-foreign`, and Avalonia's Wayland backend implements the *export* half
  of that protocol but not the import half. The handle is logged and otherwise ignored until that lands upstream.
- **Capture is through shared memory unless `capture.dmabuf` is on**, so a frame is still a GPU readback, and at 1440p60
  that is the cost that remains. What is *not* there any more is a copy on top of it: this backend allocates the buffers
  itself in both modes, and the compositor renders into the very memory the consumer reads.

  That took two attempts to get right, and the failed one is worth recording. Capturing straight into a PipeWire buffer
  was blamed on a deadlock between the compositor's repaint clock and PipeWire's graph cycle, and replaced with a
  per-frame `memcpy` out of a private buffer. The real cause was a missing `process` hook (see `cast/stream.rs`); once
  that was fixed the direct path was safe, and the copy went with it.
