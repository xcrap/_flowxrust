//! Opening a changed file in the user's editor.
//!
//! Agents write the files the inspector shows, so "open" cannot mean "hand it
//! to its default application": the default for an `.app`, `.command` or
//! `.terminal` file is to *run* it. Only passive viewer types (images, PDFs,
//! media) keep their default handler. Everything else goes to the user's
//! source-code editor, then their plain-text editor, which is the Swift
//! original's `urlForApplication(toOpen: .sourceCode)` rule.

use std::ffi::OsString;
use std::path::Path;

/// Extensions whose default handler only displays them.
const PASSIVE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "tif", "tiff", "bmp", "ico", "pdf", "mp4",
    "mov", "m4v", "mp3", "m4a", "wav", "aif", "aiff",
];

/// Whether the default handler for this file only shows it.
pub fn is_passive_viewer_type(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            PASSIVE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Arguments for `/usr/bin/open`.
///
/// `editor` is the bundle identifier of the user's editor, when one is
/// registered. Without one, `-t` asks for the default text editor, which can
/// never execute the file either.
pub fn open_arguments(path: &Path, editor: Option<&str>) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = Vec::new();
    if !is_passive_viewer_type(path) {
        match editor {
            Some(bundle) => arguments.extend([OsString::from("-b"), OsString::from(bundle)]),
            None => arguments.push(OsString::from("-t")),
        }
    }
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
    let editor = (!is_passive_viewer_type(path))
        .then(default_editor)
        .flatten();
    let mut child = std::process::Command::new("/usr/bin/open")
        .args(open_arguments(path, editor.as_deref()))
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
            assert_eq!(
                open_arguments(&path, None)[0],
                "-t",
                "{name} must open as text"
            );
            assert_eq!(
                open_arguments(&path, Some("com.example.editor"))[..2],
                ["-b", "com.example.editor"],
                "{name} must open in the editor"
            );
        }
    }

    #[test]
    fn passive_types_keep_their_viewer() {
        for name in ["logo.PNG", "doc.pdf", "clip.mov"] {
            let path = Path::new("/project").join(name);
            assert_eq!(
                open_arguments(&path, Some("com.example.editor")),
                [path.as_os_str()]
            );
        }
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(open_in_editor(Path::new("relative.txt")).is_err());
    }
}
