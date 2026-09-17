// SPDX-License-Identifier: GPL-3.0-or-later
//! Asking `wlrix-file-picker` which file.
//!
//! The same shape as [`crate::picker`] and [`crate::shot`], deliberately: a helper spawned for
//! one job, a JSON manifest on its stdin, a JSON answer on its stdout, and an exit code saying
//! which of accepted, canceled and failed happened. Both signals are checked -- a helper that
//! dies mid-answer produces neither valid stdout nor a zero exit, and either alone is enough to
//! keep this from reporting a file the user never chose.
//!
//! Read through calloop rather than waited on, because a file dialog waits on a person for as
//! long as they take, and this process still has to answer `Request.Close` and service any cast
//! already running while it does.
//!
//! ## Why a whole application rather than a dialog in here
//!
//! Because a file chooser is a file manager with one button on it. `wlrix-file-picker` is built
//! on `Wlrix.Files.Core`, the half of the wlRIX file manager that carries no window: the same
//! directory reader, the same MIME database, the same icon theme and the same bookmarks. A
//! second implementation in this process would be a worse file manager that disagreed with the
//! real one about what a file is called and which icon it has.

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
const TOOL: &str = "wlrix-file-picker";

/// Which of the interface's three calls this is.
///
/// The strings are the wire; `Wlrix.Files.Core.Portal.FileChooserMode` pins the same three on
/// the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Open,
    Save,
    SaveFiles,
}

/// One line of a file filter: a glob, or a MIME type.
#[derive(Debug, Serialize)]
pub struct Rule {
    /// Whether `pattern` is a MIME type. The interface's own `u` discriminator, 0 or 1.
    pub mime: bool,
    pub pattern: String,
}

/// A named set of rules, as one entry of the combo box.
#[derive(Debug, Serialize)]
pub struct Filter {
    pub name: String,
    pub rules: Vec<Rule>,
}

/// One option of a combo box the application asked for.
#[derive(Debug, Serialize)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
}

/// An extra control the application asked to have in the dialog.
#[derive(Debug, Serialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// Empty means a checkbox rather than a combo box, which is the interface's own rule.
    pub options: Vec<ChoiceOption>,
    pub default: String,
}

/// What the picker is asked.
#[derive(Debug, Serialize)]
pub struct Manifest {
    pub mode: Mode,
    /// The application requesting the file. Empty for an unsandboxed one, which is most of them.
    pub app_id: String,
    /// The dialog's title, chosen by the application.
    pub title: String,
    pub accept_label: String,
    pub multiple: bool,
    pub directory: bool,
    pub current_name: String,
    /// Already decoded. The interface carries this as a null-terminated `ay`, because a
    /// filename on Linux is bytes; anything that is not valid UTF-8 is dropped here rather than
    /// handed on as something the picker would have to guess at.
    pub current_folder: String,
    pub current_file: String,
    /// The names `SaveFiles` wants written into the chosen folder.
    pub files: Vec<String>,
    pub filters: Vec<Filter>,
    /// Which filter to start on, as an index into `filters`, or -1.
    ///
    /// An index rather than the filter itself, so the answer can name one without the two sides
    /// having to compare structures. A `current_filter` the application sent that is not in its
    /// own list is appended to the list, which is what keeps this an index in every case.
    pub current_filter: i32,
    pub choices: Vec<Choice>,
}

/// One answered choice.
#[derive(Debug, Deserialize)]
pub struct ChoiceAnswer {
    pub id: String,
    pub value: String,
}

/// What the picker answers.
#[derive(Debug, Deserialize)]
pub struct Answer {
    pub uris: Vec<String>,
    #[serde(default)]
    pub choices: Vec<ChoiceAnswer>,
    /// The filter that was showing, as an index into the manifest's list, or -1.
    #[serde(default = "no_filter")]
    pub current_filter: i32,
    #[serde(default)]
    pub writable: bool,
}

fn no_filter() -> i32 {
    -1
}

/// A dialog that has been put up and not yet answered.
pub struct Chooser {
    child: Child,
    /// Everything read from stdout so far. The answer is small; it arrives in one piece in
    /// practice, but a pipe is a stream and may not.
    output: Vec<u8>,
    /// The Request path this belongs to, so `Request.Close` can find and kill it.
    pub request: zbus::zvariant::OwnedObjectPath,
}

/// How a run ended.
pub enum Outcome {
    Chosen(Answer),
    Canceled,
    Failed(String),
}

impl Chooser {
    /// Spawn the picker and hand it the manifest.
    pub fn spawn(
        manifest: &Manifest,
        request: zbus::zvariant::OwnedObjectPath,
    ) -> Result<Self, String> {
        let json = serde_json::to_vec(manifest)
            .map_err(|err| format!("could not encode the file chooser manifest: {err}"))?;

        let mut child = Command::new(TOOL)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is inherited: the picker's own logging joins this process's in the journal.
            .spawn()
            .map_err(|err| format!("could not run {TOOL}: {err} (is it installed?)"))?;

        // Written and closed straight away. Unlike the source picker's, this manifest is not
        // bounded by anything small -- an application may pass a hundred filters, and
        // `SaveFiles` a list of names -- so it goes on a thread of its own rather than blocking
        // this one against a pipe that has filled. The join is what keeps the write ordered
        // before the spawn returns; it cannot outlive the child, whose stdin the thread owns.
        if let Some(mut stdin) = child.stdin.take() {
            let written =
                std::thread::spawn(move || stdin.write_all(&json).and_then(|()| stdin.flush()));
            match written.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    let _ = child.kill();
                    return Err(format!("could not send the request to {TOOL}: {err}"));
                }
                Err(_) => {
                    let _ = child.kill();
                    return Err(format!("the thread writing to {TOOL} panicked"));
                }
            }
        }

        tracing::info!(
            mode = ?manifest.mode,
            filters = manifest.filters.len(),
            multiple = manifest.multiple,
            directory = manifest.directory,
            "asking the user which file",
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

        // Exit 0 with nothing said is a cancel, not a broken helper. It is what an answer
        // written from a toolkit shutdown path looks like when the shutdown got there first --
        // the picker's own bug once, and the difference to the user is an error dialog for a
        // dialog they simply closed.
        if self.output.iter().all(u8::is_ascii_whitespace) {
            return Outcome::Canceled;
        }

        match serde_json::from_slice::<Answer>(&self.output) {
            // Success with nothing chosen is not a selection. Treated as a cancel rather than
            // an error: the user ended up choosing nothing, which is what canceling means.
            Ok(answer) if answer.uris.is_empty() => Outcome::Canceled,
            Ok(answer) => Outcome::Chosen(answer),
            Err(err) => Outcome::Failed(format!("could not read the answer from {TOOL}: {err}")),
        }
    }

    /// Take the dialog off the screen, because the application gave up on the request.
    pub fn cancel(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Chooser {
    /// Never outlive the question.
    ///
    /// A child process is not cleaned up by its parent going away, so without this a portal
    /// that crashes or is restarted leaves a file dialog on screen asking about a file nobody
    /// is listening for any more -- and whose answer has nowhere to go.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a URI is one this backend may hand to the frontend.
///
/// The interface is explicit: "All URIs must have the `file://` scheme". The picker only ever
/// answers with those, which is exactly why this is checked -- the picker is a separate process
/// and its answer is input like any other, the same rule [`crate::shot`] applies to the path it
/// is told to write.
pub fn is_acceptable(uri: &str) -> bool {
    // `file://` with an empty authority and an absolute path, which is the only form the
    // frontend and every application behind it understand. `file://host/path` is a valid URI
    // and names a file on another machine.
    uri.starts_with("file:///")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_uri_is_acceptable() {
        assert!(is_acceptable("file:///home/vic/notes.txt"));
        assert!(is_acceptable("file:///tmp/a%20b/%E7%94%BB.png"));
    }

    /// Everything the picker must never be able to answer with.
    #[test]
    fn nothing_else_is() {
        assert!(!is_acceptable("smb://server/share/notes.txt"));
        assert!(!is_acceptable("file://server/share/notes.txt"));
        assert!(!is_acceptable("/home/vic/notes.txt"));
        assert!(!is_acceptable(""));
    }

    /// A helper that exited 0 and said nothing has canceled, not crashed.
    #[test]
    fn silence_is_a_cancel_rather_than_a_failure() {
        // The parse would otherwise be the thing that decides, and it says "failed".
        assert!(serde_json::from_slice::<Answer>(b"").is_err());
    }

    #[test]
    fn an_answer_parses() {
        let answer: Answer = serde_json::from_str(
            r#"{"uris":["file:///a"],"choices":[{"id":"ro","value":"true"}],
                "current_filter":1,"writable":true}"#,
        )
        .unwrap();

        assert_eq!(answer.uris, ["file:///a"]);
        assert_eq!(answer.choices[0].id, "ro");
        assert_eq!(answer.choices[0].value, "true");
        assert_eq!(answer.current_filter, 1);
        assert!(answer.writable);
    }

    /// The picker omits what it has nothing to say about, and an older one may omit more.
    #[test]
    fn a_minimal_answer_parses() {
        let answer: Answer = serde_json::from_str(r#"{"uris":["file:///a"]}"#).unwrap();

        assert!(answer.choices.is_empty());
        assert_eq!(answer.current_filter, -1);
        assert!(!answer.writable);
    }

    /// The manifest's field names are the contract with the C# side.
    #[test]
    fn the_manifest_serializes_the_way_the_picker_reads_it() {
        let manifest = Manifest {
            mode: Mode::SaveFiles,
            app_id: String::new(),
            title: "Save".into(),
            accept_label: String::new(),
            multiple: false,
            directory: false,
            current_name: String::new(),
            current_folder: "/home/vic".into(),
            current_file: String::new(),
            files: vec!["a.txt".into()],
            filters: vec![Filter {
                name: "Text".into(),
                rules: vec![Rule {
                    mime: false,
                    pattern: "*.txt".into(),
                }],
            }],
            current_filter: 0,
            choices: vec![],
        };

        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains(r#""mode":"save_files""#), "{json}");
        assert!(json.contains(r#""current_folder":"/home/vic""#), "{json}");
        assert!(
            json.contains(r#""rules":[{"mime":false,"pattern":"*.txt"}]"#),
            "{json}"
        );
    }
}
