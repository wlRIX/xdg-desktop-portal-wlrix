// SPDX-License-Identifier: GPL-3.0-or-later
//! Asking `wlrix-screenshot` to take the picture.
//!
//! The same shape as [`crate::picker`], deliberately: a helper spawned for one job, a JSON
//! manifest on its stdin, a JSON answer on its stdout, and an exit code saying which of taken,
//! canceled and failed happened. Both signals are checked -- a helper that dies mid-answer
//! produces neither valid stdout nor a zero exit, and either alone is enough to keep this from
//! reporting a screenshot that did not happen.
//!
//! Read through calloop rather than waited on, for the same reason the picker is: an
//! interactive shot waits on a person, and this process still has to answer `Request.Close`
//! and service any cast already running while it does.
//!
//! ## Why the portal names the file
//!
//! A portal screenshot is not one the user asked to keep. The frontend copies it into the
//! requesting application's document store, so it belongs in `$XDG_RUNTIME_DIR` -- owned by
//! one user and cleaned up at logout -- and not in anybody's Pictures folder, which is where
//! `wlrix-screenshot` would put it if left to choose.

use std::{
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use serde::{Deserialize, Serialize};

/// The program spawned, found on `PATH`.
///
/// By name rather than an absolute path, matching how `wlrix-session` starts every other wlRIX
/// component -- and so a development build earlier in `PATH` is picked up without reinstalling.
const TOOL: &str = "wlrix-screenshot";

/// What the tool is asked.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// The application requesting the shot. Empty for an unsandboxed application, which is
    /// most of them.
    pub app_id: String,
    /// Whether the user gets to choose an area.
    pub interactive: bool,
    /// One of `org.freedesktop.impl.portal.Screenshot`'s target values, or `0` for unsaid.
    pub target: u32,
    /// Whether the pointer should be in the picture.
    pub cursor: bool,
    /// Where the file goes. Chosen here; see the module note.
    pub path: String,
}

/// What the tool answers.
#[derive(Debug, Deserialize)]
pub struct Answer {
    pub path: String,
}

/// A shot that has been asked for and not yet answered.
pub struct Shot {
    child: Child,
    /// Everything read from stdout so far. The answer is small; it arrives in one piece in
    /// practice, but a pipe is a stream and may not.
    output: Vec<u8>,
    /// The Request path this belongs to, so `Request.Close` can find and kill it.
    pub request: zbus::zvariant::OwnedObjectPath,
}

/// How a run ended.
pub enum Outcome {
    Taken(Answer),
    Canceled,
    Failed(String),
}

impl Shot {
    /// Spawn the tool and hand it the manifest.
    pub fn spawn(
        manifest: &Manifest,
        request: zbus::zvariant::OwnedObjectPath,
    ) -> Result<Self, String> {
        let json = serde_json::to_vec(manifest)
            .map_err(|err| format!("could not encode the screenshot manifest: {err}"))?;

        let mut child = Command::new(TOOL)
            .arg("--portal")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is inherited: the tool's own logging joins this process's in the journal.
            .spawn()
            .map_err(|err| format!("could not run {TOOL}: {err} (is it installed?)"))?;

        // Written and closed straight away. A blocking write is safe here because the manifest
        // is a few hundred bytes, far below a pipe's capacity, so it cannot fill the pipe and
        // stall this process against a tool that has not read yet. Closing is what tells the
        // tool the request is complete.
        if let Some(mut stdin) = child.stdin.take()
            && let Err(err) = stdin.write_all(&json)
        {
            let _ = child.kill();
            return Err(format!("could not send the request to {TOOL}: {err}"));
        }

        tracing::info!(
            interactive = manifest.interactive,
            target = manifest.target,
            "taking a screenshot",
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

    /// Read whatever is there. `true` once the tool has closed stdout and is finished.
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
                    tracing::warn!("could not read from {TOOL}: {err}");
                    return true;
                }
            }
        }
    }

    /// Reap the child and work out what it said.
    pub fn finish(mut self) -> Outcome {
        let status = match self.child.wait() {
            Ok(status) => status,
            Err(err) => return Outcome::Failed(format!("could not wait for {TOOL}: {err}")),
        };

        match status.code() {
            Some(0) => {}
            Some(1) => return Outcome::Canceled,
            Some(code) => return Outcome::Failed(format!("{TOOL} exited with {code}")),
            // Killed by a signal -- including by this program, when the application canceled.
            None => return Outcome::Canceled,
        }

        match serde_json::from_slice::<Answer>(&self.output) {
            Ok(answer) => Outcome::Taken(answer),
            Err(err) => Outcome::Failed(format!("could not read the answer from {TOOL}: {err}")),
        }
    }

    /// Take the overlay off the screen, because the application gave up on the request.
    pub fn cancel(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Shot {
    /// Never outlive the question.
    ///
    /// A child process is not cleaned up by its parent going away, so without this a portal
    /// that crashes or is restarted leaves a full-screen overlay up, frozen, over a desktop
    /// nobody can reach -- which is a great deal worse than the leftover picker dialog the
    /// same omission produced in [`crate::picker`].
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where a portal screenshot is written.
///
/// `$XDG_RUNTIME_DIR/wlrix-portal/`, the same directory the picker's previews use, and required
/// rather than falling back to `/tmp` for the same reason: it is owned by one user and cleaned
/// up at logout, where a predictable path in a world-writable directory is somewhere another
/// user can leave a symlink.
///
/// The name carries a counter as well as the pid, so two screenshots from one portal do not
/// collide -- and the old file is removed first, since the frontend has had it by then and a
/// stale one is a picture of somebody's screen left lying about.
pub fn destination(counter: u64) -> Result<PathBuf, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or("XDG_RUNTIME_DIR is not set; nowhere to put a screenshot")?
        .join("wlrix-portal");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    Ok(dir.join(format!("screenshot-{}-{counter}.png", std::process::id())))
}

/// A local path as the `file://` URI the interface answers with.
///
/// Percent-encoded, because a path is bytes and a URI is not: a directory with a space or a
/// `#` in it would otherwise produce a URI the frontend parses as something else entirely.
/// Only the unreserved set of RFC 3986 plus `/` is left alone.
pub fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(*byte as char)
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_a_plain_uri() {
        assert_eq!(
            file_uri(std::path::Path::new("/run/user/1000/wlrix-portal/a-1.png")),
            "file:///run/user/1000/wlrix-portal/a-1.png"
        );
    }

    /// The whole reason this is not `format!("file://{}", path.display())`.
    #[test]
    fn awkward_characters_are_encoded() {
        assert_eq!(
            file_uri(std::path::Path::new("/tmp/a b/c#d?e.png")),
            "file:///tmp/a%20b/c%23d%3Fe.png"
        );
    }

    /// A path is bytes, not text, and a locale that puts them there is ordinary here.
    #[test]
    fn non_ascii_survives_as_utf8_percent_escapes() {
        assert_eq!(
            file_uri(std::path::Path::new("/tmp/画")),
            "file:///tmp/%E7%94%BB"
        );
    }

    #[test]
    fn an_answer_parses() {
        let answer: Answer = serde_json::from_str(r#"{"path":"/run/x.png"}"#).unwrap();
        assert_eq!(answer.path, "/run/x.png");
    }
}
