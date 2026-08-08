// SPDX-License-Identifier: GPL-3.0-or-later
//! Live thumbnails for the source picker.
//!
//! The picker is a separate program in another language, and it binds no Wayland protocols at
//! all. So this side captures every candidate source and publishes a small image of each into
//! `$XDG_RUNTIME_DIR`; the picker maps those files and draws them. That is the whole reason the
//! split works: there is exactly one implementation of screen capture, in the language that has
//! the libraries for it.
//!
//! ## Why raw pixels rather than PNG
//!
//! No encode on this side, no decode on the other, and no image library in either. The file is
//! a small header followed by BGRA, which is what Avalonia's `WriteableBitmap` takes directly.
//!
//! ## Why round-robin, and why that is not stingy
//!
//! `ext-image-copy-capture` has no server-side downscale: a capture arrives at the source's full
//! size, so a 1440p monitor is 14 MB whatever the tile ends up being. Refreshing ten sources at
//! 4fps would be ~590 MB/s of readback to draw a grid of postage stamps.
//!
//! So one source is captured per tick and the cursor moves on. With ten sources at a 100ms tick
//! each tile refreshes about once a second, which reads as live in a picker, at a tenth of the
//! cost. The tick and the tile size are both in the config file.

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use wayland_client::QueueHandle;

use crate::{
    Portal,
    config::Config,
    wayland::{
        Wayland,
        capture::{Capture, Outcome, Purpose},
        shm,
    },
};

/// Magic at the head of a preview file: `WLRX`, little-endian.
///
/// The picker checks it before trusting anything else. These files live in a directory whose
/// name is predictable, and a reader that maps whatever it finds there and believes the
/// dimensions in it is a reader that can be made to read out of bounds.
const MAGIC: u32 = 0x5852_4c57;
/// Bumped if the layout below ever changes, so an old picker refuses a new file rather than
/// misreading it.
const VERSION: u32 = 1;
/// Bytes before the pixels. Eight `u32`s; see [`Tile::publish`].
const HEADER: usize = 32;

/// One candidate source, and the file its thumbnail is published in.
struct Tile {
    source_id: String,
    /// Where the picker reads it from.
    path: PathBuf,
    file: File,
    /// Bumped either side of a write, so a reader can tell a torn image from a settled one.
    /// Odd means "being written".
    seq: u32,
}

/// The thumbnails for one picker run.
pub struct Previews {
    /// `$XDG_RUNTIME_DIR/wlrix-portal/<session>`, removed when the picker is done.
    dir: PathBuf,
    tiles: Vec<Tile>,
    /// Whose turn it is.
    cursor: usize,
    last_tick: Instant,
    tick: Duration,
    tile_size: (u32, u32),
    /// Scratch for the full-size capture, before it is scaled down. One, reused: only one
    /// capture is ever in flight.
    scratch: Option<shm::Memory>,
}

impl Previews {
    /// Create the directory and one file per source.
    ///
    /// The files are made up front and at full size, so the picker can map every one of them at
    /// startup and simply watch for the sequence number to move.
    pub fn new(session: &str, sources: &[(String, bool)], config: &Config) -> Result<Self, String> {
        let dir = runtime_dir()?.join("wlrix-portal").join(session);
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;

        let (width, height) = config.preview.tile;
        let length = HEADER + (width as usize * height as usize * shm::BYTES_PER_PIXEL);

        let mut tiles = Vec::new();
        for (source_id, _) in sources {
            let path = dir.join(format!("preview-{}.raw", sanitize(source_id)));
            let file = File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|err| format!("could not create {}: {err}", path.display()))?;
            file.set_len(length as u64)
                .map_err(|err| format!("could not size {}: {err}", path.display()))?;
            tiles.push(Tile {
                source_id: source_id.clone(),
                path,
                file,
                seq: 0,
            });
        }

        tracing::debug!(dir = %dir.display(), tiles = tiles.len(), "publishing previews");
        Ok(Self {
            dir,
            tiles,
            cursor: 0,
            last_tick: Instant::now() - config.preview.tick,
            tick: config.preview.tick,
            tile_size: config.preview.tile,
            scratch: None,
        })
    }

    /// Where the picker should look for a source's thumbnail.
    pub fn path_for(&self, source_id: &str) -> Option<&Path> {
        self.tiles
            .iter()
            .find(|tile| tile.source_id == source_id)
            .map(|tile| tile.path.as_path())
    }

    /// Collect any finished capture and start the next one.
    ///
    /// Called once per loop iteration. Does nothing between ticks, and nothing at all while a
    /// capture is outstanding -- one at a time is the whole point.
    pub fn pump(&mut self, wayland: &mut Wayland, qh: &QueueHandle<Portal>) {
        self.collect(wayland);

        if self.tiles.is_empty() || self.last_tick.elapsed() < self.tick {
            return;
        }
        // Someone is still working: let it finish rather than piling on.
        if wayland
            .captures
            .iter()
            .any(|capture| capture.purpose == Purpose::Preview && capture.busy())
        {
            return;
        }
        self.last_tick = Instant::now();
        self.request_next(wayland, qh);
    }

    /// Publish whatever finished since the last look.
    fn collect(&mut self, wayland: &mut Wayland) {
        let Some(scratch) = &self.scratch else {
            return;
        };
        for tile in &mut self.tiles {
            let Some(capture) = wayland
                .captures
                .iter_mut()
                .find(|c| c.purpose == Purpose::Preview && c.source_id == tile.source_id)
            else {
                continue;
            };
            let (Some(outcome), Some(constraints)) = (capture.take_outcome(), capture.constraints)
            else {
                continue;
            };
            match outcome {
                Outcome::Ready => {
                    if let Err(err) = tile.publish(
                        scratch.pixels(),
                        (constraints.width, constraints.height),
                        self.tile_size,
                    ) {
                        tracing::warn!(source = %tile.source_id, "could not publish a preview: {err}");
                    }
                }
                Outcome::Failed(reason) => {
                    tracing::debug!(source = %tile.source_id, "preview frame failed: {reason}")
                }
            }
        }
    }

    /// Ask the compositor for the next tile's frame.
    fn request_next(&mut self, wayland: &mut Wayland, qh: &QueueHandle<Portal>) {
        // Try each tile once, so a source whose capture cannot be opened does not stop the
        // rotation on itself forever.
        for _ in 0..self.tiles.len() {
            let index = self.cursor % self.tiles.len();
            self.cursor = self.cursor.wrapping_add(1);
            let source_id = self.tiles[index].source_id.clone();

            let Some(position) = wayland
                .captures
                .iter()
                .position(|c| c.purpose == Purpose::Preview && c.source_id == source_id)
            else {
                continue;
            };
            let Some(constraints) = wayland.captures[position].constraints else {
                // The compositor has not said how big this source is yet. It will.
                continue;
            };

            let needed = shm::Buffer::size_for(constraints.width as i32, constraints.height as i32);
            let grow = self
                .scratch
                .as_ref()
                .is_none_or(|s| s.pixels().len() < needed);
            if grow {
                match shm::Memory::new(needed) {
                    Ok(memory) => self.scratch = Some(memory),
                    Err(err) => {
                        tracing::warn!("could not allocate preview memory: {err}");
                        return;
                    }
                }
            }
            let Some(scratch) = &self.scratch else {
                return;
            };
            let Some(shm_global) = &wayland.shm else {
                return;
            };

            // A fresh `wl_buffer` per request, because the scratch is shared between sources of
            // different sizes. Protocol objects are cheap; a buffer per source at full size is
            // not -- ten 1440p monitors would be 140 MB standing.
            let buffer = shm::Buffer::new(
                shm_global,
                qh,
                shm::Region::whole(scratch.as_fd(), scratch.pixels().len()),
                constraints,
            );
            wayland.captures[position].request(qh, &buffer.buffer, buffer.width, buffer.height);
            return;
        }
    }

    /// Open capture sessions for every source being previewed.
    pub fn open_captures(&self, wayland: &mut Wayland, qh: &QueueHandle<Portal>) {
        let Some(copy) = wayland.copy.clone() else {
            return;
        };
        for tile in &self.tiles {
            if wayland
                .captures
                .iter()
                .any(|c| c.purpose == Purpose::Preview && c.source_id == tile.source_id)
            {
                continue;
            }
            let source = match wayland.inventory.monitor(&tile.source_id) {
                Some(monitor) => wayland.monitor_source(qh, monitor),
                None => wayland
                    .inventory
                    .window(&tile.source_id)
                    .and_then(|window| wayland.window_source(qh, window)),
            };
            let Some(source) = source else {
                continue;
            };
            // Never with the cursor: a thumbnail is for recognizing a window, and the pointer
            // is somewhere else entirely by the time anyone looks at it.
            wayland.captures.push(Capture::new(
                &copy,
                qh,
                source,
                tile.source_id.clone(),
                Purpose::Preview,
                false,
            ));
        }
    }

    /// Stop previewing and take the files away.
    pub fn close(self, wayland: &mut Wayland) {
        wayland
            .captures
            .retain(|capture| capture.purpose != Purpose::Preview);
        // Best effort: a leftover file in the runtime directory is cleaned up at logout anyway,
        // and failing to remove one is not worth failing a screen share over.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Tile {
    /// Scale the capture down into this tile's file.
    ///
    /// The sequence number is written odd before the pixels and even after, so a reader that
    /// sees an odd value, or a different value either side of its read, knows to try again
    /// rather than draw half of one frame and half of the next.
    fn publish(
        &mut self,
        pixels: &[u8],
        source: (u32, u32),
        tile: (u32, u32),
    ) -> std::io::Result<()> {
        self.seq = self.seq.wrapping_add(1) | 1; // odd: writing
        self.write_header(source, tile)?;

        let scaled = scale(pixels, source, tile);
        self.file.seek(SeekFrom::Start(HEADER as u64))?;
        self.file.write_all(&scaled)?;

        self.seq = self.seq.wrapping_add(1); // even: settled
        self.write_header(source, tile)?;
        self.file.flush()?;
        Ok(())
    }

    fn write_header(&mut self, source: (u32, u32), tile: (u32, u32)) -> std::io::Result<()> {
        // Eight little-endian u32s: magic, version, width, height, stride, format, seq, and one
        // reserved so the pixels start 32-byte aligned.
        let mut header = [0u8; HEADER];
        let fields = [
            MAGIC,
            VERSION,
            tile.0,
            tile.1,
            tile.0 * shm::BYTES_PER_PIXEL as u32,
            // 0 means "BGRA/BGRX 8888", the only thing this ever writes. A named constant here
            // rather than a bare zero would suggest there is a choice; there is not, yet.
            0,
            self.seq,
            // Reserved: the source's own size, which the picker uses to letterbox a tile whose
            // aspect ratio does not match.
            source.0 << 16 | (source.1 & 0xffff),
        ];
        for (slot, value) in header.chunks_exact_mut(4).zip(fields) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)
    }
}

/// Nearest-neighbor downscale.
///
/// Good enough for a thumbnail and cheap, which matters when this runs on the same thread as
/// everything else. A box filter would look better on text-heavy windows and is the obvious
/// improvement if the tiles ever look bad.
fn scale(pixels: &[u8], source: (u32, u32), tile: (u32, u32)) -> Vec<u8> {
    let bpp = shm::BYTES_PER_PIXEL;
    let mut out = vec![0u8; tile.0 as usize * tile.1 as usize * bpp];
    if source.0 == 0 || source.1 == 0 {
        return out;
    }
    let source_stride = source.0 as usize * bpp;
    for y in 0..tile.1 as usize {
        // `+ source/2` rounds to the nearest source pixel rather than always the one above-left,
        // which otherwise shifts the whole image half a tile-pixel up and to the left.
        let sy = (y * source.1 as usize + source.1 as usize / 2) / tile.1 as usize;
        let sy = sy.min(source.1 as usize - 1);
        for x in 0..tile.0 as usize {
            let sx = (x * source.0 as usize + source.0 as usize / 2) / tile.0 as usize;
            let sx = sx.min(source.0 as usize - 1);
            let from = sy * source_stride + sx * bpp;
            let to = (y * tile.0 as usize + x) * bpp;
            if let (Some(src), Some(dst)) =
                (pixels.get(from..from + bpp), out.get_mut(to..to + bpp))
            {
                dst.copy_from_slice(src);
            }
        }
    }
    out
}

/// Make a source id safe to put in a filename.
///
/// Foreign-toplevel identifiers are opaque strings from the compositor and output names come
/// from EDID; neither is promised to be free of `/` or `..`. Anything not plainly safe becomes
/// an underscore, which can collide -- harmlessly, since a collision only means two sources
/// share a tile file, and the manifest tells the picker which path belongs to which source.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The per-user runtime directory, which is where anything this transient belongs.
///
/// Required rather than falling back to `/tmp`: it is owned by one user and cleaned up at
/// logout, where a predictable path in a world-writable directory is somewhere another user can
/// leave a symlink.
fn runtime_dir() -> Result<PathBuf, String> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set; cannot publish previews".to_string())
}
