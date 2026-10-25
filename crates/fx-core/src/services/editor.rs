//! Opening a changed file in the user's editor.
//!
//! Agents write the files the inspector shows, so "open" cannot mean "hand it
//! to its default application": the default for an `.app`, `.command` or
//! `.terminal` file is to *run* it. Only passive viewer types (images, PDFs,
//! media) keep their default handler. Everything else goes to the user's
//! source-code editor, then their plain-text editor, which is the Swift
//! original's `urlForApplication(toOpen: .sourceCode)` rule.
//!
//! The name is not the file, so the extension alone decides nothing. A
//! `shot.png` that is a symlink to `evil.command` launches the command, and a
//! real `.png` carrying a per-file "Open With" attribute opens in whatever
//! that names, Terminal included. So a passive type keeps its viewer only when
//! the path itself is a regular file, and the viewer is always named with
//! `-b`: given a bundle, `open` uses it and never reads the file's own choice.

use std::ffi::OsString;
use std::path::Path;

/// Extensions whose default handler only displays them, with the content type
/// that handler is registered for.
const PASSIVE_TYPES: &[(&str, &str)] = &[
    ("aif", "public.aiff-audio"),
    ("aiff", "public.aiff-audio"),
    ("bmp", "com.microsoft.bmp"),
    ("gif", "com.compuserve.gif"),
    ("heic", "public.heic"),
    ("heif", "public.heif"),
    ("ico", "com.microsoft.ico"),
    ("jpeg", "public.jpeg"),
    ("jpg", "public.jpeg"),
    ("m4a", "com.apple.m4a-audio"),
    ("m4v", "com.apple.m4v-video"),
    ("mov", "com.apple.quicktime-movie"),
    ("mp3", "public.mp3"),
    ("mp4", "public.mpeg-4"),
    ("pdf", "com.adobe.pdf"),
    ("png", "public.png"),
    ("tif", "public.tiff"),
    ("tiff", "public.tiff"),
    ("wav", "com.microsoft.waveform-audio"),
    ("webp", "org.webmproject.webp"),
];

/// The content type of a passive viewer type, read from the extension.
pub fn passive_content_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    PASSIVE_TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, content_type)| *content_type)
}

/// Whether the default handler for this file's type only shows it.
pub fn is_passive_viewer_type(path: &Path) -> bool {
    passive_content_type(path).is_some()
}

/// The application that opens a file. Always named, never left to `open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opener {
    /// The passive viewer registered for the file's type, by bundle id.
    Viewer(String),
    /// The user's source-code or plain-text editor, by bundle id.
    Editor(String),
    /// No editor is registered: `open -t`, the default text editor.
    TextEditor,
}

/// Choose the application for `path`.
///
/// `regular_file` is whether the path itself, without following a link, is a
/// plain file. Anything else — a symlink, a directory, a bundle — goes to the
/// editor whatever it is called. `viewer` resolves a content type's default
/// handler and `editor` the user's editor; both are passed in so the choice
/// can be tested without Launch Services.
pub fn choose_opener(
    path: &Path,
    regular_file: bool,
    viewer: impl FnOnce(&str) -> Option<String>,
    editor: impl FnOnce() -> Option<String>,
) -> Opener {
    let viewer = passive_content_type(path)
        .filter(|_| regular_file)
        .and_then(viewer);
    match (viewer, editor()) {
        (Some(bundle), _) => Opener::Viewer(bundle),
        (None, Some(bundle)) => Opener::Editor(bundle),
        (None, None) => Opener::TextEditor,
    }
}

/// Arguments for `/usr/bin/open`.
///
/// Every opener names its application, so a per-file "Open With" choice is
/// never read. `-t` asks for the default text editor, which can never execute
/// the file either.
pub fn open_arguments(path: &Path, opener: &Opener) -> Vec<OsString> {
    let mut arguments = match opener {
        Opener::Viewer(bundle) | Opener::Editor(bundle) => {
            vec![OsString::from("-b"), OsString::from(bundle)]
        }
        Opener::TextEditor => vec![OsString::from("-t")],
    };
    arguments.push(path.as_os_str().to_owned());
    arguments
}

/// The application registered for source code, then for plain text.
pub fn default_editor() -> Option<String> {
    ["public.source-code", "public.plain-text"]
        .into_iter()
        .find_map(default_handler)
}

/// Open `path` without blocking: `open` is reaped on a short-lived thread.
pub fn open_in_editor(path: &Path) -> std::io::Result<()> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only an absolute path can be opened",
        ));
    }
    let regular_file = std::fs::symlink_metadata(path)?.file_type().is_file();
    let opener = choose_opener(path, regular_file, default_handler, default_editor);
    let mut child = std::process::Command::new("/usr/bin/open")
        .args(open_arguments(path, &opener))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn default_handler(content_type: &str) -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultRoleHandlerForContentType(
            content_type: CFStringRef,
            role: u32,
        ) -> CFStringRef;
    }
    const ALL_ROLES: u32 = 0xFFFF_FFFF;

    let content_type = CFString::new(content_type);
    // SAFETY: the argument is a live CFString; the result follows the create
    // rule and is released when the wrapper drops.
    let handler = unsafe {
        LSCopyDefaultRoleHandlerForContentType(content_type.as_concrete_TypeRef(), ALL_ROLES)
    };
    if handler.is_null() {
        return None;
    }
    let handler = unsafe { CFString::wrap_under_create_rule(handler) }.to_string();
    (!handler.is_empty()).then_some(handler)
}

#[cfg(not(target_os = "macos"))]
fn default_handler(_: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewer(content_type: &str) -> Option<String> {
        Some(format!("viewer.for.{content_type}"))
    }

    fn editor() -> Option<String> {
        Some("com.example.editor".into())
    }

    #[test]
    fn executable_types_never_reach_their_default_handler() {
        for name in [
            "tool.command",
            "Thing.app",
            "run.terminal",
            "script.sh",
            "main.rs",
        ] {
            let path = Path::new("/project").join(name);
            let opener = choose_opener(&path, true, viewer, || None);
            assert_eq!(
                open_arguments(&path, &opener)[0],
                "-t",
                "{name} must open as text"
            );
            let opener = choose_opener(&path, true, viewer, editor);
            assert_eq!(
                open_arguments(&path, &opener)[..2],
                ["-b", "com.example.editor"],
                "{name} must open in the editor"
            );
        }
    }

    #[test]
    fn passive_types_keep_their_viewer_named_explicitly() {
        // Named with `-b`: a bare `open logo.png` reads the file's own "Open
        // With" attribute, which can name Terminal.
        for (name, content_type) in [
            ("logo.PNG", "public.png"),
            ("doc.pdf", "com.adobe.pdf"),
            ("clip.mov", "com.apple.quicktime-movie"),
        ] {
            let path = Path::new("/project").join(name);
            let bundle = format!("viewer.for.{content_type}");
            let opener = choose_opener(&path, true, viewer, editor);
            assert_eq!(opener, Opener::Viewer(bundle.clone()));
            assert_eq!(
                open_arguments(&path, &opener),
                [
                    OsString::from("-b"),
                    OsString::from(bundle),
                    path.as_os_str().to_owned(),
                ]
            );
        }
    }

    #[test]
    fn a_passive_name_that_is_not_a_regular_file_goes_to_the_editor() {
        let path = Path::new("/project/shot.png");
        assert_eq!(
            choose_opener(path, false, viewer, editor),
            Opener::Editor("com.example.editor".into())
        );
        assert_eq!(
            choose_opener(path, false, viewer, || None),
            Opener::TextEditor
        );
    }

    #[test]
    fn a_passive_type_with_no_viewer_goes_to_the_editor() {
        let path = Path::new("/project/shot.png");
        assert_eq!(
            choose_opener(path, true, |_| None, editor),
            Opener::Editor("com.example.editor".into())
        );
    }

    #[test]
    fn a_symlink_named_like_an_image_is_not_a_regular_file() {
        // `shot.png -> evil.command`: the link's name says image, and its
        // default handler would have run the target in Terminal.
        let directory = tempfile::tempdir().expect("a temp directory");
        let target = directory.path().join("evil.command");
        std::fs::write(&target, "#!/bin/sh\n").expect("written");
        let link = directory.path().join("shot.png");
        std::os::unix::fs::symlink(&target, &link).expect("linked");
        let regular_file = std::fs::symlink_metadata(&link)
            .expect("the link")
            .file_type()
            .is_file();
        assert!(!regular_file, "the link read as a regular file");
        assert_eq!(
            choose_opener(&link, regular_file, viewer, editor),
            Opener::Editor("com.example.editor".into())
        );
    }

    #[test]
    fn the_passive_table_names_each_extension_once() {
        let mut extensions: Vec<_> = PASSIVE_TYPES
            .iter()
            .map(|(extension, _)| *extension)
            .collect();
        let count = extensions.len();
        extensions.sort_unstable();
        extensions.dedup();
        assert_eq!(extensions.len(), count);
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(open_in_editor(Path::new("relative.txt")).is_err());
    }
}
