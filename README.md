# xdg-desktop-portal-wlrix

The wlRIX desktop portal backend. Implements `org.freedesktop.impl.portal.ScreenCast` and
`org.freedesktop.impl.portal.Screenshot`, which is what `xdg-desktop-portal` hands an application's screen-sharing and
screenshot requests to — so that Firefox, OBS and anything else that asks for a screen gets one — plus
`org.freedesktop.impl.portal.Settings`, which is how a GTK or Qt application is told what the desktop looks like, and
`org.freedesktop.impl.portal.FileChooser`, which is the file dialog every sandboxed application opens.

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
| **this**                  | Rust. D-Bus, PipeWire, and the capture behind a cast.                                  |
| **`wlrix-source-picker`** | C#/Avalonia, from `wlrix-apps`. The dialog that asks which monitor or window to share. |
| **`wlrix-screenshot`**    | Rust, its own repo. Takes the picture for `Screenshot`, region overlay and all.        |
| **`wlrix-file-picker`**   | C#/Avalonia, from `wlrix-apps`. The file dialog behind `FileChooser`.                  |

The split is not about taste. A ScreenCast portal has to produce a PipeWire stream, and PipeWire has no C# bindings —
SPA's POD builders are `static inline` in the C headers with no exported symbols, so they cannot even be reached by
P/Invoke. Rust has the PipeWire project's own bindings. Conversely the picker wants a themed dialog with a grid of live
previews, which is an afternoon in Avalonia against the `Wlrix.Avalonia` theme and a great deal longer in a hand-rolled
toolkit.

So the daemon captures everything and publishes preview frames into `$XDG_RUNTIME_DIR`; the picker reads them and draws.
The picker binds no Wayland protocols at all.

`wlrix-screenshot` is a third program and the split there is different again: it is spawned for the whole job, capture
included, rather than only for the dialog. A screenshot needs a fullscreen region overlay, which needs layer shell,
which the tool already has — and this backend would otherwise gain a PNG encoder, a file-naming policy and a second
implementation of the region UI for no gain. It is spawned exactly as the picker is; see
[The screenshot contract](#the-screenshot-contract).

Internally one calloop loop owns all state. D-Bus runs on its own thread and reports in through a `calloop::channel` —
the shape `wlrix-idle` uses — so there is no shared mutable state and no lock between the two.

## Build

```sh
cargo build
```

Needs `libpipewire-0.3`, `libspa-0.2` and `libgbm` development files. (On Debian and Ubuntu:
`libpipewire-0.3-dev libspa-0.2-dev libgbm-dev`.) Missing any of them shows up only as a link error from `cargo build` —
`cargo clippy` never links, so it passes regardless.

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

**Exit code** — `0` accepted, `1` canceled, anything else failed. Both signals are checked: a picker that dies
mid-answer produces neither valid stdout nor a zero exit. An empty `sources` with exit 0 counts as canceled, and so
does **no output at all** — that is what an answer written from a toolkit shutdown path looks like when the shutdown
gets there first, which `wlrix-source-picker` did until 2026-09-17, so declining to share showed the application a
failure instead of a cancel. Ids not in the manifest are discarded, and more ids than `multiple` allows are truncated.

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

## The screenshot contract

`wlrix-screenshot` is spawned with `--portal` for one picture and exits with the answer. The same shape as the picker's
contract above, deliberately — one kind of helper, one thing to remember.

**In** — a JSON manifest on stdin, closed straight after:

```json
{
  "app_id": "org.gnome.Screenshot",
  "interactive": true,
  "target": 4,
  "cursor": false,
  "path": "/run/user/1000/wlrix-portal/screenshot-4213-1.png"
}
```

`target` is the interface's own value: `1` screen, `4` area. `0` means the application did not say, which the tool
treats as an area. `cursor` is always `false` — the interface has no key for it, so this backend has nothing to say, and
the tool ORs it with its own `screenshot.toml` rather than giving the same setting two homes that can disagree.

**Out** — on stdout, when the picture was taken:

```json
{
  "path": "/run/user/1000/wlrix-portal/screenshot-4213-1.png"
}
```

**Exit code** — `0` taken, `1` canceled, anything else failed. Both are checked, as with the picker. Exit 0 with
nothing on stdout is read as a **cancel** rather than a failure, which is the rule all three helper contracts share: it
is what an answer written from a toolkit shutdown path looks like when the shutdown gets there first, and the difference
to the user is an error dialog for something they simply dismissed.

**This backend names the file, and the answer may only confirm it.** A tool answering with a different path is refused
rather than passed on: the helper is a separate process and its answer is input, and a URI for a file this backend never
asked to exist is how a helper turns into a way to read somebody else's files.

The file goes in `$XDG_RUNTIME_DIR/wlrix-portal/` rather than in the user's Pictures directory, because a portal
screenshot is not one the user asked to keep — the frontend hands it to the requesting application. What the frontend
does with it afterwards is not documented, so rather than guess, this backend removes the previous file when the next
screenshot is taken and the last one when it stops: one file per running portal, whichever the answer turns out to be.

## The file chooser contract

`wlrix-file-picker` is spawned for one dialog and exits with the answer — the same shape as the picker and the
screenshot tool, deliberately. It is a **C# application built on `Wlrix.Files.Core`**, the half of the wlRIX file
manager that carries no window: the same directory reader, the same MIME database, the same icon theme and the same
`bookmarks.json`. A second implementation in this process would be a worse file manager that disagreed with the real
one about what a file is called and which icon it has.

**In** — a JSON manifest on stdin, closed straight after:

```json
{
  "mode": "open",
  "app_id": "org.mozilla.firefox",
  "title": "Upload File",
  "accept_label": "_Upload",
  "multiple": true,
  "directory": false,
  "current_name": "",
  "current_folder": "/home/vic/Pictures",
  "current_file": "",
  "files": [],
  "filters": [{ "name": "Images", "rules": [{ "mime": false, "pattern": "*.png" },
                                            { "mime": true,  "pattern": "image/jpeg" }] }],
  "current_filter": 0,
  "choices": [{ "id": "ro", "label": "Open read-only", "options": [], "default": "false" }]
}
```

`mode` is `open`, `save` or `save_files`, one per interface method. `current_folder`, `current_file` and each entry of
`files` arrive on D-Bus as **null-terminated `ay` byte arrays** — a filename on Linux is bytes and need not be valid
UTF-8 — and are decoded here; one that will not decode is dropped rather than mangled, because a mangled path is a
dialog opening somewhere that does not exist while looking as though it worked.

`current_filter` is an **index** into `filters`, not the filter itself, so the answer can name one back without the two
programs comparing structures across a JSON boundary. An application is allowed to send a `current_filter` that is not
in its own `filters`, and that one is appended to the list here — which is what keeps this an index in every case.

A choice with an empty `options` list is a **checkbox** whose value is the string `"true"` or `"false"`. That is the
interface's own rule, not a convention invented here.

**Out** — on stdout, when the user accepted:

```json
{
  "uris": ["file:///home/vic/Pictures/holiday.png"],
  "choices": [{ "id": "ro", "value": "false" }],
  "current_filter": 0,
  "writable": false
}
```

**Exit code** — `0` accepted, `1` canceled, anything else failed. Both are checked, as with the other two helpers, and
an accepted answer with an empty `uris` — or no output at all — is read as a cancel: the user ended up choosing nothing, which is what
canceling means, and an application should not be shown a failure for it.

**The answer is input, and is checked.** Every URI must be `file:///…` — the interface requires it — and a `save_files`
answer must have exactly one URI per name it was asked about, in order, because the application matches the two lists
by position. A bad answer is refused **whole** rather than filtered: a shorter list than the user chose is an
application silently attaching three files out of four, which is worse than a failure they can see.

## Settings

`org.freedesktop.impl.portal.Settings` is the only way to tell a toolkit two things, and one of them cannot be said
anywhere else at all.

| Namespace | Key | What wlRIX reports |
|-----------|-----|--------------------|
| `org.gnome.desktop.wm.preferences` | `button-layout` | `menu:minimize,maximize` |
| `org.freedesktop.appearance` | `color-scheme` | 1 for a dark scheme, 2 for a light one |
| `org.freedesktop.appearance` | `accent-color` | the palette's accent, as `(ddd)` |
| `org.freedesktop.appearance` | `contrast` | 0 — IRIX schemes are not contrast variants |

**The button layout has no other route.** On Wayland GTK reads `gtk-decoration-layout` from this interface and ignores
`settings.ini` for it; without a backend it keeps its own compiled `menu:close`. Measured both ways. The layout reported
here is what `wlrix-compositor`'s own `FrameStyle` has — a menu button, minimize and maximize, and no close, because
IRIX kept Close in the window menu. A window framed by the compositor still has one: right-click anywhere on the border,
or press Alt+F4.

Two asymmetries worth knowing, both verified against a live GTK:

- **GTK 3 only asks a portal when `GTK_USE_PORTAL=1`.** `wlrix-session`'s `start-wlrix.sh` exports it for exactly this
  reason. GTK 4 asks unconditionally.
- **GTK will not draw the menu button.** GTK 3 draws one only for an application that has an app menu; GTK 4 leaves the
  leading `windowcontrols` empty. It is asked for anyway — it costs nothing and it says what the layout means.

Everything this backend does not own answers `NotFound`, deliberately. Claiming an interface in `wlrix-portals.conf`
takes the *whole* interface, so a key invented here is a key the user can no longer set: the font and the cursor theme
are the ones that would hurt, and both are left alone.

The scheme comes from `[appearance] palette` in `portal.toml`, which `wlrix-settings-daemon` writes alongside the four
components that draw. It is re-read on every call, so an application started after a change is told the new scheme with
nothing having to signal this process.

`SettingChanged` is declared and never emitted, which is a decision rather than an omission — see the note at the top of
`src/dbus/settings.rs`. The short version: the colors reach GTK through `gtk.css`, GTK does not reload that file, and
flipping only the dark hint would repaint half a window.

## Configuration

`$XDG_CONFIG_HOME/wlrix/portal.toml`, else `/etc/wlrix/portal.toml`. There is no file by default and the defaults are
what everything was tuned with. Unknown keys are an error, as elsewhere in wlRIX.

```toml
[preview]
tick_ms = 100     # how often *one* source is captured; they take turns
tile = [320, 180] # thumbnail size

[capture]
dmabuf = false    # offer GPU memory as well as shared memory — see below

[appearance]
palette = "gotham" # the session's color scheme, for what Settings reports
```

`[appearance] palette` is not tuning and is not normally hand-edited: `wlrix-settings-daemon` writes it from
`appearance.palette` along with the compositor's, the desktop's, the tray's and the screenshot tool's, so a GTK
application is told the same scheme the chrome around it is drawn in.

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

**ScreenCast**, interface version 4. Monitor and window sources; hidden and embedded cursor modes.

**Screenshot**, interface version 3, with `AvailableTargets` = `Screen | Area`. Both go through `wlrix-screenshot`; the
area target is its region overlay.

**FileChooser**, interface version 3 — `OpenFile`, `SaveFile` **and** `SaveFiles`, all three through
`wlrix-file-picker`. All three, because claiming an interface claims every method on it: an application calling
`SaveFiles` against a backend that implemented only the other two would get a D-Bus error where `default=gtk` used to
give it a working dialog.

Not implemented, and deliberately not advertised:

- **`Screenshot`'s `Window` and `ActiveWindow` targets.** `Window` means one the user picks, which needs a window picker
  the tool does not have. `ActiveWindow` needs the focused window's **frame** rectangle, and no client can work that
  out — the compositor draws wlRIX's 4Dwm frames outside the window's own surface tree, so a per-toplevel capture comes
  back with no titlebar. The compositor hands that rectangle to `wlrix-screenshot` directly for Alt+Print, and nothing
  reaches it from here.
- **`PickColor`.** The method is served, because a backend that claims an interface serves all of it — a frontend
  calling a method that is simply absent gets a D-Bus error rather than a portal response the application knows how to
  show — but it answers "ended". Claiming `Screenshot` takes `PickColor` with it: `portals.conf` names whole interfaces,
  and there is no way to hand one method back to another backend.

- **`FileChooser`'s `modal` option.** Read and ignored. Making the dialog modal to the application that asked needs the
  `parent_window` handle to reach the toolkit putting it up, and under Wayland that is an `xdg_foreign` exported handle
  Avalonia cannot import — the same gap the source picker has, recorded under Known gaps.

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

**ScreenCast** works end to end. Verified on real hardware with **OBS** under a wlRIX session on the KVM: monitor and
window sources, the picker with live previews, and a stream that survives the shared window being resized.

**Screenshot** is verified end to end against the nested compositor: a `Screen` request answers `Success` with a
`file://` URI, and the file behind it is a real 1280×800 screenshot of the desktop. The failure paths were exercised
against a stand-in helper on a private bus — a helper answering with a path it was not asked for is refused with
`Ended`, a helper reporting a cancel comes back as `Canceled` rather than an error, and the runtime directory is empty
after the backend stops. Not yet done with a real application asking through the frontend, and `Area` has only been
driven by hand rather than through `interactive: true` from a browser.

**FileChooser** is verified end to end on a private bus against the nested compositor, with the picker on `PATH`:
`OpenFile` with two filters, a `current_filter` and a checkbox put the dialog up, and answered `Success` with two
`file://` URIs, the chosen filter **returned as the tuple the caller sent**, `writable: false`, and the choice. `SaveFile`
canceled by the user came back `Canceled` rather than an error; `Request.Close` mid-dialog killed the picker and
answered `Canceled`; and `SaveFiles` with two names — one of them already on disk — answered two URIs in the order
asked, the taken one renamed. The one thing not yet done is a real application asking through the frontend, which needs
`sudo just install`.

Testing the D-Bus half needs `PIPEWIRE_RUNTIME_DIR` pointed at the real one when the nested rig moves
`XDG_RUNTIME_DIR`: this backend takes the bus name only after *both* the compositor and PipeWire are up, so a
screenshot-only test still fails at PipeWire without it.

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
5. **`PickColor`.** The one method this backend serves and does not implement. `wlrix-screenshot`'s overlay already
   holds the frozen pixels, so it is a crosshair, a zoom loupe and a click away — and until it exists, an application
   offering "pick a color" under wlRIX gets a failure.
6. **RemoteDesktop** — blocked on the compositor: there is no `zwlr_virtual_pointer_v1`, and virtual-keyboard is behind
   the sandbox check.

## Known gaps

- **Neither helper dialog is parented to the window that asked.** The portal's
  `parent_window` is a `wayland:` handle for `xdg-foreign`, and Avalonia's Wayland backend implements the *export* half
  of that protocol but not the import half. The handle is logged and otherwise ignored until that lands upstream. It is
  the source picker's one visible flaw and the file chooser's: `FileChooser`'s `modal` option cannot be honored for the
  same reason.
- **Capture is through shared memory unless `capture.dmabuf` is on**, so a frame is still a GPU readback, and at 1440p60
  that is the cost that remains. What is *not* there any more is a copy on top of it: this backend allocates the buffers
  itself in both modes, and the compositor renders into the very memory the consumer reads.

  That took two attempts to get right, and the failed one is worth recording. Capturing straight into a PipeWire buffer
  was blamed on a deadlock between the compositor's repaint clock and PipeWire's graph cycle, and replaced with a
  per-frame `memcpy` out of a private buffer. The real cause was a missing `process` hook (see `cast/stream.rs`); once
  that was fixed the direct path was safe, and the copy went with it.
