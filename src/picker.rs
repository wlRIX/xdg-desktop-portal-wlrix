// SPDX-License-Identifier: GPL-3.0-or-later
//! Asking the user which screen or window to share.
//!
//! The picker is a separate program -- `wlrix-source-picker`, the Avalonia app in `wlrix-apps`.
//! It is spawned for one question and exits with the answer, which keeps the whole of the UI
//! toolkit out of this process and lets the dialog be written in the language the rest of the
//! wlRIX desktop is written in.
//!
//! ## The contract
//!
//! - A JSON manifest on the picker's **stdin**, closed immediately after: what was asked for,
//!   and every source that could answer it, each with the path to its live thumbnail.
//! - The selection as JSON on **stdout** when the user accepts.
//! - The **exit code** says which happened: `0` accepted, `1` canceled, anything else failed.
//!   Belt and braces on purpose -- a picker that dies mid-answer produces no stdout *and* a
//!   non-zero code, and either alone is enough to keep the portal from reporting a share that
//!   is not happening.
//! - stderr is the picker's log and goes to the journal with everything else.
//!
//! ## Why it is read through calloop rather than waited on
//!
//! The user may take a minute, and this process still has to answer `Request.Close`, keep the
//! thumbnails moving and service any cast already running. So the child's stdout is just
//! another event source on the loop, and the answer arrives when it arrives.

use std::{
    io::Write,
    os::fd::AsFd,
    process::{Child, Command, Stdio},
};

use serde::{Deserialize, Serialize};

/// The program spawned, found on `PATH`.
///
/// By name rather than an absolute path, matching how `wlrix-session` starts every other wlRIX
/// component -- and so a development build earlier in `PATH` is picked up without reinstalling.
const PICKER: &str = "wlrix-source-picker";

/// The Wayland app id the picker sets on its window.
///
/// Used to keep it out of the list of windows it is offering. Must match the `AppId` in the
/// picker's own `Program.cs`; there is no way for the two to check each other.
pub const APP_ID: &str = "com.wlrix.sourcepicker";

/// What the picker is asked.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// The application requesting the share, for the dialog's wording. Empty for an
    /// unsandboxed application, which is most of them.
    pub app_id: String,
    /// Whether more than one source may be chosen.
    pub multiple: bool,
    /// Whether the cursor will be in the stream, so the dialog can say so.
    pub cursor: bool,
    pub sources: Vec<Source>,
}

/// One thing that could be shared.
#[derive(Debug, Serialize)]
pub struct Source {
    /// Opaque to the picker; it comes back verbatim in the selection.
    pub id: String,
    /// `"monitor"` or `"window"`, which is how the dialog groups them into tabs.
    pub kind: &'static str,
    /// What to show a person.
    pub label: String,
    /// The application, for a window. Empty for a monitor.
    pub app_id: String,
    /// The source's own size, so a tile can letterbox rather than stretch.
    pub width: u32,
    pub height: u32,
    /// The live thumbnail file. Absent if none could be published, in which case the picker
    /// shows the tile without a preview rather than refusing to offer the source.
    pub preview: Option<String>,
}

/// What the picker answers.
#[derive(Debug, Deserialize)]
pub struct Selection {
    /// Ids from the manifest, in the order the user picked them.
    pub sources: Vec<String>,
}

/// A picker that has been asked and not yet answered.
pub struct Picker {
    child: Child,
    /// Everything read from stdout so far. The answer is small; it arrives in one piece in
    /// practice, but a pipe is a stream and may not.
    output: Vec<u8>,
    /// The Request path this picker belongs to, so `Request.Close` can find and kill it.
    pub request: zbus::zvariant::OwnedObjectPath,
}

/// How a picker run ended.
pub enum Outcome {
    Accepted(Selection),
    Canceled,
    Failed(String),
}

impl Picker {
    /// Spawn the picker and hand it the manifest.
    pub fn spawn(
        manifest: &Manifest,
        request: zbus::zvariant::OwnedObjectPath,
    ) -> Result<Self, String> {
        let json = serde_json::to_vec(manifest)
            .map_err(|err| format!("could not encode the picker manifest: {err}"))?;

        let mut child = Command::new(PICKER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is inherited: the picker's own logging joins this process's in the journal.
            .spawn()
            .map_err(|err| format!("could not run {PICKER}: {err} (is it installed?)"))?;

        // Written and closed straight away. A blocking write is safe here because the manifest
        // is far below a pipe's capacity -- a handful of sources, a few hundred bytes each --
        // so it cannot fill the pipe and stall this process against a picker that has not read
        // yet. Closing is what tells the picker the list is complete.
        if let Some(mut stdin) = child.stdin.take()
            && let Err(err) = stdin.write_all(&json)
        {
            let _ = child.kill();
            return Err(format!("could not send the manifest to {PICKER}: {err}"));
        }

        tracing::info!(
            sources = manifest.sources.len(),
            multiple = manifest.multiple,
            "asking the user which source to share",
        );
        Ok(Self {
            child,
            output: Vec::new(),
            request,
        })
    }

    /// The child's stdout, to be watched on the loop.
    ///
    /// Taken, so this can only happen once -- the source owns the fd from then on.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Read whatever is there. `true` once the picker has closed stdout and is finished.
    ///
    /// Reads the descriptor directly rather than through `std::io::Read`: calloop hands its
    /// callbacks a `NoIoDrop`, which derefs immutably only -- it exists to stop a source's fd
    /// being closed out from under the loop -- so there is no `&mut ChildStdout` to be had.
    pub fn read_available(&mut self, fd: impl AsFd) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            match rustix::io::read(fd.as_fd(), &mut chunk) {
                Ok(0) => return true,
                Ok(read) => self.output.extend_from_slice(&chunk[..read]),
                // Nothing more for now; the loop will call back when there is.
                Err(rustix::io::Errno::AGAIN) => return false,
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => {
                    tracing::warn!("could not read from {PICKER}: {err}");
                    return true;
                }
            }
        }
    }

    /// Reap the child and work out what it said.
    pub fn finish(mut self) -> Outcome {
        let status = match self.child.wait() {
            Ok(status) => status,
            Err(err) => return Outcome::Failed(format!("could not wait for {PICKER}: {err}")),
        };

        match status.code() {
            Some(0) => {}
            Some(1) => return Outcome::Canceled,
            Some(code) => return Outcome::Failed(format!("{PICKER} exited with {code}")),
            // Killed by a signal -- including by this program, when the application canceled.
            None => return Outcome::Canceled,
        }

        match serde_json::from_slice::<Selection>(&self.output) {
            // Success with an empty list is not a selection. Treated as a cancel rather than an
            // error: the user ended up sharing nothing, which is what cancelling means, and an
            // application should not show them a failure for it.
            Ok(selection) if selection.sources.is_empty() => Outcome::Canceled,
            Ok(selection) => Outcome::Accepted(selection),
            Err(err) => Outcome::Failed(format!("could not read the answer from {PICKER}: {err}")),
        }
    }

    /// Take the picker down, because the application gave up on the request behind it.
    pub fn cancel(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Picker {
    /// Never outlive the question.
    ///
    /// A child process is not cleaned up by its parent going away, so without this a portal
    /// that crashes or is restarted leaves a dialog on screen asking about a share nobody is
    /// listening for any more -- and whose answer has nowhere to go. Seen exactly once during
    /// development, which was once more than enough.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
