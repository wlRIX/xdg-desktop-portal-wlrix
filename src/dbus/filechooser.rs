// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.impl.portal.FileChooser`: the interface `xdg-desktop-portal` hands a file
//! dialog to.
//!
//! Three methods, and **all three have to work**. `data/wlrix-portals.conf` says it already
//! about Screenshot and PickColor: claiming an interface claims every method on it, and there
//! is no way to hand one back to another backend. An application calling `SaveFiles` against a
//! backend that implements only the other two gets a D-Bus error where it used to get a
//! working dialog from `default=gtk`.
//!
//! ## What is not implemented, and is answered honestly
//!
//! `modal` is read and ignored. Making a dialog modal to the application that asked needs the
//! parent window handle in `parent_window` to reach the toolkit putting the dialog up, and
//! under Wayland that is an `xdg_foreign` exported handle -- which Avalonia has no way to
//! import. The dialog is an ordinary window instead. Saying so here is the point: it is a
//! difference somebody will notice, and it is not an oversight.

use std::collections::HashMap;

use zbus::{
    ObjectServer,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use super::{PortalResponse, Request, oneshot, request};
use crate::filechooser::Mode;

/// Which revision of the interface this implements.
///
/// 3, the revision `SaveFiles` and `current_filter` belong to. Both are real here.
const VERSION: u32 = 3;

/// A filter as the interface carries it: a name, and rules of (kind, pattern).
///
/// Kept in the D-Bus shape rather than converted, because the answer has to name one *back* in
/// exactly this shape -- so the list the application sent is what the result is built from, and
/// nothing can be lost in a round trip through a type of our own.
pub type FilterTuple = (String, Vec<(u32, String)>);

/// A combo box as the interface carries it: id, label, options of (id, label), and a default.
pub type ChoiceTuple = (String, String, Vec<(String, String)>, String);

/// What a file dialog was asked for, carried to the main loop.
#[derive(Debug, Clone, Default)]
pub struct ChooserOptions {
    pub accept_label: String,
    pub multiple: bool,
    pub directory: bool,
    pub current_name: String,
    pub current_folder: String,
    pub current_file: String,
    /// `SaveFiles`' list of names to write.
    pub files: Vec<String>,
    pub filters: Vec<FilterTuple>,
    /// An index into `filters`, or -1. See [`ChooserOptions::parse`] for why an index.
    pub current_filter: i32,
    pub choices: Vec<ChoiceTuple>,
}

impl ChooserOptions {
    /// Read the options vardict, ignoring anything unrecognized.
    ///
    /// Lenient on purpose, and the opposite of how wlRIX reads its *config* files: an unknown
    /// key there is a typo worth an error, but here it is a newer frontend passing an option
    /// from a later revision of the interface, and refusing the call would break file dialogs
    /// on an upgrade that changed nothing about wlRIX.
    pub fn parse(options: &HashMap<String, OwnedValue>) -> Self {
        let mut parsed = Self {
            current_filter: -1,
            ..Self::default()
        };

        if let Some(label) = options.get("accept_label").and_then(string) {
            parsed.accept_label = label;
        }
        if let Some(multiple) = options.get("multiple").and_then(|v| bool::try_from(v).ok()) {
            parsed.multiple = multiple;
        }
        if let Some(directory) = options
            .get("directory")
            .and_then(|v| bool::try_from(v).ok())
        {
            parsed.directory = directory;
        }
        if let Some(name) = options.get("current_name").and_then(string) {
            parsed.current_name = name;
        }
        if let Some(folder) = options.get("current_folder").and_then(path) {
            parsed.current_folder = folder;
        }
        if let Some(file) = options.get("current_file").and_then(path) {
            parsed.current_file = file;
        }
        if let Some(files) = options.get("files").and_then(paths) {
            parsed.files = files;
        }
        if let Some(filters) = options
            .get("filters")
            .and_then(|v| Vec::<FilterTuple>::try_from(Value::from(v.clone())).ok())
        {
            parsed.filters = filters;
        }
        if let Some(choices) = options
            .get("choices")
            .and_then(|v| Vec::<ChoiceTuple>::try_from(Value::from(v.clone())).ok())
        {
            parsed.choices = choices;
        }

        // The current filter travels as an **index** into the list above, so the picker can
        // name one back without the two programs having to compare structures across a JSON
        // boundary. An application is allowed to send a `current_filter` that is not in its own
        // `filters` -- GTK's own documentation says so -- and that case is handled by appending
        // it, which is what keeps this an index in every case rather than an index most of the
        // time and a special case the once.
        if let Some(current) = options
            .get("current_filter")
            .and_then(|v| FilterTuple::try_from(Value::from(v.clone())).ok())
        {
            parsed.current_filter = match parsed.filters.iter().position(|f| *f == current) {
                Some(index) => index as i32,
                None => {
                    parsed.filters.push(current);
                    parsed.filters.len() as i32 - 1
                }
            };
        }

        parsed
    }
}

/// A string option, or none if it was not one.
fn string(value: &OwnedValue) -> Option<String> {
    String::try_from(value.clone()).ok()
}

/// A path option: a **null-terminated byte array**, which is what the interface uses for
/// anything that is a filename.
///
/// A filename on Linux is bytes and need not be valid UTF-8, which is why the interface does
/// not carry these as strings. One that is not decodable is dropped rather than mangled: the
/// picker would have nothing to show for it, and a mangled path is a dialog opening somewhere
/// that does not exist while looking as though it worked.
fn path(value: &OwnedValue) -> Option<String> {
    let bytes = Vec::<u8>::try_from(Value::from(value.clone())).ok()?;
    decode(&bytes)
}

/// A list of those, for `SaveFiles`.
fn paths(value: &OwnedValue) -> Option<Vec<String>> {
    let arrays = Vec::<Vec<u8>>::try_from(Value::from(value.clone())).ok()?;
    Some(arrays.iter().filter_map(|bytes| decode(bytes)).collect())
}

/// Strips the terminator and decodes.
fn decode(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8(bytes[..end].to_vec()).ok()?;
    (!text.is_empty()).then_some(text)
}

/// What the main loop answers a file dialog with.
#[derive(Debug, Default)]
pub struct ChooserResult {
    /// `file://` URIs, already checked. See [`crate::filechooser::is_acceptable`].
    pub uris: Vec<String>,
    pub choices: Vec<(String, String)>,
    /// The filter that was showing, in the shape the application sent it.
    pub current_filter: Option<FilterTuple>,
    pub writable: bool,
}

pub struct FileChooser {
    pub sender: calloop::channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    /// Choose one or more existing files, or a folder.
    async fn open_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        self.choose(
            server,
            Mode::Open,
            handle,
            app_id,
            parent_window,
            title,
            options,
        )
        .await
    }

    /// Name a file to write.
    async fn save_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        self.choose(
            server,
            Mode::Save,
            handle,
            app_id,
            parent_window,
            title,
            options,
        )
        .await
    }

    /// Choose a folder for a set of files whose names the application already knows.
    async fn save_files(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        self.choose(
            server,
            Mode::SaveFiles,
            handle,
            app_id,
            parent_window,
            title,
            options,
        )
        .await
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        VERSION
    }
}

impl FileChooser {
    /// The whole of all three methods, which differ only in the mode they ask for.
    #[allow(clippy::too_many_arguments)]
    async fn choose(
        &self,
        server: &ObjectServer,
        mode: Mode,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let parsed = ChooserOptions::parse(&options);
        tracing::debug!(
            app_id = %app_id,
            request = %handle,
            parent_window = %parent_window,
            ?mode,
            "FileChooser",
        );

        // The Request object is how the application cancels while the dialog is up -- a browser
        // tab closed mid-upload arrives here. Exported before anything slow starts, and removed
        // however this ends.
        let request_object = request::PortalRequest {
            path: handle.clone(),
            sender: self.sender.clone(),
        };
        if let Err(err) = server.at(&handle, request_object).await {
            tracing::error!("could not export the request object: {err}");
            return (PortalResponse::Ended as u32, HashMap::new());
        }

        let (reply, receiver) = oneshot();
        // Awaiting rather than blocking: a file dialog waits on a person, and a blocked handler
        // would hold zbus's executor and stop `Request.Close` being dispatched -- so canceling
        // would only take effect after the thing it cancels had finished. See the [`super`]
        // module documentation.
        let (response, result) = if self
            .sender
            .send(Request::FileChooser {
                request: handle.clone(),
                app_id,
                title,
                mode,
                options: parsed,
                reply,
            })
            .is_err()
        {
            tracing::error!("the main loop is gone; failing the call");
            (PortalResponse::Ended, ChooserResult::default())
        } else {
            receiver
                .recv()
                .await
                .unwrap_or((PortalResponse::Ended, ChooserResult::default()))
        };

        let _ = server.remove::<request::PortalRequest, _>(&handle).await;

        if response != PortalResponse::Success {
            return (response as u32, HashMap::new());
        }

        let mut results = HashMap::from([(
            "uris".to_string(),
            OwnedValue::try_from(Value::from(result.uris)).expect("an array of strings"),
        )]);
        if !result.choices.is_empty()
            && let Ok(value) = OwnedValue::try_from(Value::from(result.choices))
        {
            results.insert("choices".to_string(), value);
        }
        if let Some(filter) = result.current_filter
            && let Ok(value) = OwnedValue::try_from(Value::from(filter))
        {
            results.insert("current_filter".to_string(), value);
        }
        // Only for an open: the interface lists `writable` under `OpenFile` alone, and a save
        // is writable by definition.
        if mode == Mode::Open {
            results.insert(
                "writable".to_string(),
                OwnedValue::try_from(Value::from(result.writable)).expect("a bool"),
            );
        }

        (PortalResponse::Success as u32, results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    }

    fn owned(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).expect("representable")
    }

    #[test]
    fn the_defaults_are_a_plain_open() {
        let parsed = ChooserOptions::parse(&HashMap::new());
        assert!(!parsed.multiple);
        assert!(!parsed.directory);
        assert_eq!(parsed.current_filter, -1);
        assert!(parsed.filters.is_empty());
    }

    /// A key from a later revision of the interface must not break a file dialog.
    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = ChooserOptions::parse(&options(vec![
            ("multiple", owned(Value::from(true))),
            ("something_new", owned(Value::from(1u32))),
        ]));
        assert!(parsed.multiple);
    }

    /// The whole reason paths are not strings on this interface.
    #[test]
    fn a_folder_arrives_as_null_terminated_bytes() {
        let parsed = ChooserOptions::parse(&options(vec![(
            "current_folder",
            owned(Value::from(b"/home/vic/Pictures\0".to_vec())),
        )]));
        assert_eq!(parsed.current_folder, "/home/vic/Pictures");
    }

    /// A filename is bytes and need not decode. Dropped rather than mangled: a mangled path
    /// opens somewhere that does not exist while looking as though it worked.
    #[test]
    fn a_path_that_is_not_utf8_is_dropped() {
        let parsed = ChooserOptions::parse(&options(vec![(
            "current_folder",
            owned(Value::from(vec![0x2fu8, 0xff, 0xfe, 0x00])),
        )]));
        assert_eq!(parsed.current_folder, "");
    }

    #[test]
    fn save_files_names_arrive_as_an_array_of_byte_arrays() {
        let parsed = ChooserOptions::parse(&options(vec![(
            "files",
            owned(Value::from(vec![b"a.txt\0".to_vec(), b"b.txt\0".to_vec()])),
        )]));
        assert_eq!(parsed.files, ["a.txt", "b.txt"]);
    }

    #[test]
    fn filters_survive_as_the_interface_sent_them() {
        let filters: Vec<FilterTuple> = vec![
            (
                "Images".into(),
                vec![(0, "*.png".into()), (1, "image/jpeg".into())],
            ),
            ("All".into(), vec![(0, "*".into())]),
        ];
        let parsed = ChooserOptions::parse(&options(vec![(
            "filters",
            owned(Value::from(filters.clone())),
        )]));

        assert_eq!(parsed.filters, filters);
        // Nothing said which one, so the picker starts on its own choice.
        assert_eq!(parsed.current_filter, -1);
    }

    #[test]
    fn the_current_filter_becomes_an_index_into_the_list() {
        let filters: Vec<FilterTuple> = vec![
            ("Images".into(), vec![(0, "*.png".into())]),
            ("Text".into(), vec![(0, "*.txt".into())]),
        ];
        let parsed = ChooserOptions::parse(&options(vec![
            ("filters", owned(Value::from(filters))),
            (
                "current_filter",
                owned(Value::from((
                    "Text".to_string(),
                    vec![(0u32, "*.txt".to_string())],
                ))),
            ),
        ]));

        assert_eq!(parsed.current_filter, 1);
        assert_eq!(parsed.filters.len(), 2);
    }

    /// An application may send a current_filter that is not in its own list. Appending it is
    /// what keeps this an index in every case rather than an index most of the time.
    #[test]
    fn a_current_filter_that_is_not_in_the_list_joins_it() {
        let parsed = ChooserOptions::parse(&options(vec![
            (
                "filters",
                owned(Value::from(vec![(
                    "Images".to_string(),
                    vec![(0u32, "*.png".to_string())],
                )])),
            ),
            (
                "current_filter",
                owned(Value::from((
                    "Text".to_string(),
                    vec![(0u32, "*.txt".to_string())],
                ))),
            ),
        ]));

        assert_eq!(parsed.filters.len(), 2);
        assert_eq!(parsed.current_filter, 1);
        assert_eq!(parsed.filters[1].0, "Text");
    }

    #[test]
    fn choices_survive_as_the_interface_sent_them() {
        let choices: Vec<ChoiceTuple> = vec![
            (
                "encoding".into(),
                "Encoding".into(),
                vec![("utf8".into(), "UTF-8".into())],
                "utf8".into(),
            ),
            // No options: a checkbox, which is the interface's own rule.
            ("ro".into(), "Read only".into(), vec![], "false".into()),
        ];
        let parsed = ChooserOptions::parse(&options(vec![(
            "choices",
            owned(Value::from(choices.clone())),
        )]));

        assert_eq!(parsed.choices, choices);
    }
}
