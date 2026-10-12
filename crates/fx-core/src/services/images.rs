//! Preparing images before they are attached to a prompt.
//!
//! An image reaches a provider as inline bytes plus a mime type, and both have
//! to be right: a wrong type is silently dropped by the model, and the bytes
//! ride along in every subsequent turn of the thread. So this module answers two
//! questions — what is this file, and is it safe to read — before either is
//! handed to a transcript.
//!
//! Three decisions are worth stating outright:
//!
//! - **The format comes from the bytes, never from the filename.** `photo.png`
//!   that is really a JPEG is a real case (a download saved twice, an export
//!   with the wrong suffix), and the extension is the only thing the user chose.
//! - **A read is bounded by the policy, not by the file.** The size rule has to
//!   apply without first reading a file to find out how big it is, or a file the
//!   policy exists to refuse has already been paid for in memory.
//! - **Nothing here decodes pixels.** See [`ImagePolicy::max_dimension`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::models::Attachment;

/// PNG's signature.
///
/// Eight bytes, not four: the trailing `\x1a\x0a` is what keeps a text file that
/// happens to begin with the same three characters from being accepted.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// JPEG's signature. Three bytes — the third is a marker, not a length.
const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];

const GIF87A_MAGIC: &[u8; 6] = b"GIF87a";
const GIF89A_MAGIC: &[u8; 6] = b"GIF89a";

/// WebP's container is RIFF, which WAV and AVI share.
const RIFF_MAGIC: &[u8; 4] = b"RIFF";

/// The RIFF form type that makes a container a WebP. It is a second chunk,
/// starting at byte 8 — a size field sits between it and `RIFF`.
const WEBP_FORM_TYPE: &[u8; 4] = b"WEBP";

/// How much of an unrecognised file an error reports.
const SNIFFED_PREVIEW_BYTES: usize = 16;

/// Default ceiling on one attached image.
///
/// A turn carries its images inline, so this is bounded by what a provider
/// accepts in a request rather than by what a disk holds. 5 MiB sits well above a
/// full-resolution screenshot and well below the size at which a turn is
/// refused outright, which would cost the user the whole message.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// How many image reads one batch keeps in flight.
///
/// A paste or a multi-select can name dozens of files at once, and each read is
/// blocking IO. Left unbounded, tokio's blocking pool runs all of them at once —
/// a thundering herd against one disk, and one slow volume delaying every image
/// behind it. Four overlaps enough to hide latency without making the paste feel
/// like it is competing with the user's own work.
pub const MAX_CONCURRENT_IMAGE_READS: usize = 4;

/// An image format FlowX can name from its leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    /// The type sent to the provider.
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }

    /// The suffix this format is normally stored under.
    ///
    /// Only for naming a file FlowX writes itself. An attachment keeps the name
    /// the user gave it: correcting a mislabelled suffix would hide from them
    /// that the file was not what they thought it was.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }
}

/// Identify a format from the leading bytes alone.
///
/// Returns `None` for anything unrecognized rather than guessing from a
/// filename. The signatures are not interchangeable and none of them is a prefix
/// of another, so the order of the checks is a matter of taste rather than of
/// correctness.
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&PNG_MAGIC) {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(&JPEG_MAGIC) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(GIF87A_MAGIC) || bytes.starts_with(GIF89A_MAGIC) {
        return Some(ImageFormat::Gif);
    }
    // The `WEBP` check is separate from the `RIFF` one because `RIFF` alone is
    // also WAV, AVI and AVIF — accepting it would take every one of them for an
    // image, and then send `image/webp` for a sound file.
    if bytes.starts_with(RIFF_MAGIC)
        && bytes.len() >= RIFF_MAGIC.len() + 4 + WEBP_FORM_TYPE.len()
        && &bytes[8..12] == WEBP_FORM_TYPE
    {
        return Some(ImageFormat::WebP);
    }
    None
}

/// How much of an image FlowX will read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePolicy {
    /// Ceiling on the bytes read for one image.
    pub max_bytes: usize,
    /// Ceiling on width and height in pixels.
    ///
    /// Always `None`, and nothing reads a value set here. Declared because the
    /// check is genuinely wanted — a panorama pasted from a phone is a
    /// legitimate prompt that the provider will refuse — and left unimplemented
    /// because doing it honestly needs a decoder, which this crate deliberately
    /// does not take. A hand-rolled PNG header parse would enforce the limit on
    /// PNGs and quietly not enforce it on the other three, which is worse than
    /// not enforcing it at all: the gap would be invisible.
    pub max_dimension: Option<u32>,
}

impl ImagePolicy {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            max_dimension: None,
        }
    }
}

impl Default for ImagePolicy {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_IMAGE_BYTES)
    }
}

/// Why an image could not be attached.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("{} could not be read: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is over the {limit} byte limit", path.display())]
    TooLarge {
        path: PathBuf,
        /// Bytes read before the bounded read stopped.
        ///
        /// Not the file's length: the read is capped one byte past the limit, so
        /// this is `limit + 1` for any file larger than the policy allows. The
        /// alternative — reporting a size obtained by reading a file the policy
        /// exists to avoid reading — would be a number obtained by the very
        /// means it promises not to use.
        bytes: usize,
        limit: usize,
    },
    #[error("{} is not a recognised image (it begins with {sniffed:?})", path.display())]
    Unrecognised {
        path: PathBuf,
        /// The leading bytes, so the user can see what they actually attached.
        sniffed: Vec<u8>,
    },
    #[error("{} is not a file", path.display())]
    NotAFile { path: PathBuf },
    #[error("{} is empty", path.display())]
    Empty { path: PathBuf },
}

/// An image that passed the policy, ready to attach.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub attachment: Attachment,
    pub format: ImageFormat,
    pub bytes: usize,
}

/// One image's outcome within a batch.
///
/// A batch does not fail as a whole: one file the user cannot open must not stop
/// the rest from attaching, and the composer has to be able to say *which* file
/// it was.
#[derive(Debug)]
pub struct PreparedImageResult {
    pub path: PathBuf,
    pub result: Result<PreparedImage, ImageError>,
}

impl PreparedImageResult {
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// The attachment, or `None` for the image that failed.
    pub fn attachment(&self) -> Option<&Attachment> {
        self.result
            .as_ref()
            .ok()
            .map(|prepared| &prepared.attachment)
    }

    pub fn into_result(self) -> Result<PreparedImage, ImageError> {
        self.result
    }
}

/// Read one image under the default policy.
pub fn prepare(path: &Path) -> Result<PreparedImage, ImageError> {
    prepare_with(path, &ImagePolicy::default())
}

/// Read one image under a given policy.
pub fn prepare_with(path: &Path, policy: &ImagePolicy) -> Result<PreparedImage, ImageError> {
    // `metadata` follows symlinks, so a link to a directory is caught here too.
    let metadata = std::fs::metadata(path).map_err(|source| ImageError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;
    // A directory is its own error rather than an unreadable one: opening it
    // yields EISDIR on some paths and succeeds on others, and "that folder is
    // not an image" is a different thing to tell the user than "that folder
    // could not be read".
    if !metadata.is_file() {
        return Err(ImageError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    let data = read_bounded(path, policy.max_bytes)?;
    let filename = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    };
    finish(data, path.to_path_buf(), filename, policy)
}

/// Prepare an image already in memory, as a paste or a drop delivers it.
pub fn prepare_bytes(data: Vec<u8>, filename: &str) -> Result<PreparedImage, ImageError> {
    prepare_bytes_with(data, filename, &ImagePolicy::default())
}

/// Prepare an in-memory image under a given policy.
pub fn prepare_bytes_with(
    data: Vec<u8>,
    filename: &str,
    policy: &ImagePolicy,
) -> Result<PreparedImage, ImageError> {
    // The filename stands in for the path in errors: a paste has no path, and
    // the name is the only thing that identifies which attachment failed.
    finish(data, PathBuf::from(filename), filename.to_string(), policy)
}

/// Read at most one byte past the limit.
///
/// The extra byte is what distinguishes a file exactly at the limit from one over
/// it, without either being read whole. A file that outruns the limit is never
/// read past it, whatever its real length turns out to be — including one that
/// grew between the metadata check and the open.
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, ImageError> {
    let file = std::fs::File::open(path).map_err(|source| ImageError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|source| ImageError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(data)
}

/// Apply the policy and build the attachment.
fn finish(
    data: Vec<u8>,
    path: PathBuf,
    filename: String,
    policy: &ImagePolicy,
) -> Result<PreparedImage, ImageError> {
    if data.len() > policy.max_bytes {
        return Err(ImageError::TooLarge {
            path,
            bytes: data.len(),
            limit: policy.max_bytes,
        });
    }
    // Checked before sniffing so the error says what is actually wrong. "That
    // file is empty" is a fact about the file; "that is not an image" sends the
    // user looking for the wrong problem.
    if data.is_empty() {
        return Err(ImageError::Empty { path });
    }
    let Some(format) = sniff(&data) else {
        let end = SNIFFED_PREVIEW_BYTES.min(data.len());
        return Err(ImageError::Unrecognised {
            path,
            sniffed: data[..end].to_vec(),
        });
    };
    let bytes = data.len();
    Ok(PreparedImage {
        // The mime type comes from the bytes, never from the filename; only the
        // name itself is carried through unchanged.
        attachment: Attachment::new(data, format.mime_type(), filename),
        format,
        bytes,
    })
}

/// Read one image on a blocking thread, under the default policy.
///
/// The caller is a UI thread. A file read there stalls every frame behind it,
/// and the file may be on a volume that is slow or asleep.
pub async fn prepare_async(path: &Path) -> Result<PreparedImage, ImageError> {
    prepare_async_with(path, &ImagePolicy::default()).await
}

/// Read one image on a blocking thread, under a given policy.
pub async fn prepare_async_with(
    path: &Path,
    policy: &ImagePolicy,
) -> Result<PreparedImage, ImageError> {
    let requested = path.to_path_buf();
    let path = requested.clone();
    let policy = policy.clone();
    match tokio::task::spawn_blocking(move || prepare_with(&path, &policy)).await {
        Ok(result) => result,
        // The read touches no shared state, so the closure cannot panic from a
        // poisoned lock; a join failure is a runtime fault rather than an image
        // fault, and it still has to reach the caller as something it can show.
        Err(_) => Err(ImageError::Unreadable {
            path: requested,
            source: std::io::Error::other("the image reader task did not finish"),
        }),
    }
}

/// Prepare several images at once, reporting each one's outcome.
///
/// Results come back in the order the paths were given, so a composer can attach
/// what worked and mark what did not without re-matching paths to positions.
pub async fn prepare_many(paths: &[PathBuf], policy: &ImagePolicy) -> Vec<PreparedImageResult> {
    let policy = policy.clone();
    let results = map_bounded(paths.to_vec(), move |path: PathBuf| {
        let policy = policy.clone();
        async move {
            let result = prepare_async_with(&path, &policy).await;
            (path, result)
        }
    })
    .await;

    results
        .into_iter()
        .map(|(_, (path, result))| PreparedImageResult { path, result })
        .collect()
}

/// Run `work` over `items`, at most [`MAX_CONCURRENT_IMAGE_READS`] at a time.
///
/// Split out from [`prepare_many`] so the bound can be tested by measuring what
/// is in flight, rather than by timing real reads and hoping.
async fn map_bounded<T, R, F, Fut>(items: Vec<T>, work: F) -> Vec<(usize, R)>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_IMAGE_READS));
    let work = Arc::new(work);
    let mut tasks = JoinSet::new();

    for (index, item) in items.into_iter().enumerate() {
        // Acquired here rather than inside the task: holding the permit is what
        // bounds the reads, and a permit waiting to be acquired has not started
        // one.
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            // Only a closed semaphore fails here, and nothing closes it while
            // these tasks are alive. Stopping rather than unwrapping keeps a
            // future change from turning an unreachable case into a panic.
            Err(_) => break,
        };
        let work = Arc::clone(&work);
        tasks.spawn(async move {
            let _permit = permit;
            (index, work(item).await)
        });
    }

    let mut results: Vec<(usize, R)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        // A task that panicked is dropped rather than taking the batch with it.
        // Preparation is entirely fallible IO and cannot panic, so this covers a
        // bug in a future change rather than anything reachable today.
        if let Ok(result) = joined {
            results.push(result);
        }
    }
    // Completion order is whatever the disk did; the caller's order is the one
    // the user chose.
    results.sort_by_key(|(index, _)| *index);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    // Headers only. Nothing here decodes pixels, so a signature is all any of
    // these functions ever look at.
    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\n".to_vec()
    }

    fn jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0]
    }

    fn gif87a() -> Vec<u8> {
        b"GIF87a\x01\x00".to_vec()
    }

    fn gif89a() -> Vec<u8> {
        b"GIF89a\x01\x00".to_vec()
    }

    fn webp() -> Vec<u8> {
        b"RIFF\x24\x00\x00\x00WEBPVP8 ".to_vec()
    }

    fn text() -> Vec<u8> {
        b"this is a text file, honestly".to_vec()
    }

    fn write(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn policy(max_bytes: usize) -> ImagePolicy {
        ImagePolicy::new(max_bytes)
    }

    #[test]
    fn each_format_is_recognised_by_its_own_signature() {
        let cases = [
            (png(), ImageFormat::Png, "image/png", "png"),
            (jpeg(), ImageFormat::Jpeg, "image/jpeg", "jpg"),
            (gif87a(), ImageFormat::Gif, "image/gif", "gif"),
            (gif89a(), ImageFormat::Gif, "image/gif", "gif"),
            (webp(), ImageFormat::WebP, "image/webp", "webp"),
        ];
        for (bytes, format, mime, extension) in cases {
            assert_eq!(sniff(&bytes), Some(format), "{bytes:?} was misread");
            assert_eq!(format.mime_type(), mime);
            assert_eq!(format.extension(), extension);
        }
    }

    #[test]
    fn a_png_needs_all_eight_of_its_signature_bytes() {
        // Seven bytes is one short, and a mangled byte in the middle must not
        // pass on the strength of the ones around it.
        assert_eq!(sniff(b"\x89PNG\r\n\x1a"), None);
        assert_eq!(sniff(b"\x89PNX\r\n\x1a\n"), None);
        assert_eq!(sniff(b"\x89PNGr\n\x1a\n"), None);
        assert_eq!(sniff(b"PNG\r\n\x1a\n...."), None);
    }

    #[test]
    fn a_jpeg_signature_is_three_bytes_and_not_two() {
        // `\xFF\xD8` alone also begins an MP2 audio frame.
        assert_eq!(sniff(&[0xFF, 0xD8]), None);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF]), Some(ImageFormat::Jpeg));
    }

    #[test]
    fn riff_without_the_webp_form_type_is_not_an_image() {
        // `RIFF` is WAV, AVI and AVIF too. Taking it for an image would send
        // `image/webp` alongside a sound file.
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00AVI LIST"), None);
        assert_eq!(sniff(&webp()), Some(ImageFormat::WebP));
    }

    #[test]
    fn the_webp_form_type_is_read_at_its_own_offset() {
        // Truncated before byte 12: the container is there but the form type that
        // distinguishes WebP from every other RIFF file is not.
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00"), None);
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WEB"), None);
        // And not one byte earlier than that.
        assert_eq!(sniff(b"WEBP\x24\x00\x00\x00RIFF"), None);
    }

    #[test]
    fn bytes_that_are_not_an_image_are_rejected_rather_than_guessed_at() {
        assert_eq!(sniff(&[]), None);
        assert_eq!(sniff(&text()), None);
        assert_eq!(sniff(&[0x00; 32]), None);
    }

    #[test]
    fn a_directory_is_reported_as_not_a_file() {
        // Reading one is EISDIR on some paths and succeeds on others, so it
        // cannot be left to surface as a generic read failure.
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("folder");
        std::fs::create_dir(&directory).unwrap();

        match prepare(&directory) {
            Err(ImageError::NotAFile { path }) => assert_eq!(path, directory),
            other => panic!("expected NotAFile, got {other:?}"),
        }
    }

    #[test]
    fn a_symlink_to_a_directory_is_reported_as_not_a_file() {
        // The metadata that decides is the one that follows the link, so a
        // folder reached through a symlink is caught rather than read.
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("folder");
        std::fs::create_dir(&directory).unwrap();
        let link = temp.path().join("looks-like-a-file.png");
        std::os::unix::fs::symlink(&directory, &link).unwrap();

        assert!(matches!(prepare(&link), Err(ImageError::NotAFile { .. })));
    }

    #[test]
    fn a_missing_file_is_reported_as_unreadable() {
        let temp = TempDir::new().unwrap();
        let absent = temp.path().join("absent.png");

        match prepare(&absent) {
            Err(ImageError::Unreadable { path, source }) => {
                assert_eq!(path, absent);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_file_is_reported_as_empty_rather_than_unrecognised() {
        // Both are true, but only one names the problem the user can act on.
        let temp = TempDir::new().unwrap();
        let empty = write(temp.path(), "empty.png", b"");

        match prepare(&empty) {
            Err(ImageError::Empty { path }) => assert_eq!(path, empty),
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn an_unrecognised_file_reports_the_bytes_it_found() {
        // The user attached the wrong file; showing them its first bytes is the
        // difference between "not an image" and "this is not what I clicked".
        let temp = TempDir::new().unwrap();
        let contents = b"%PDF-1.7 nope";
        let path = write(temp.path(), "document.png", contents);

        match prepare(&path) {
            Err(ImageError::Unrecognised {
                path: reported,
                sniffed,
            }) => {
                assert_eq!(reported, path);
                assert_eq!(sniffed, contents);
            }
            other => panic!("expected Unrecognised, got {other:?}"),
        }
    }

    #[test]
    fn an_unrecognised_file_reports_at_most_a_short_prefix() {
        let temp = TempDir::new().unwrap();
        let path = write(temp.path(), "big.bin", &vec![0xAB; 4096]);

        match prepare(&path) {
            Err(ImageError::Unrecognised { sniffed, .. }) => {
                assert_eq!(sniffed.len(), SNIFFED_PREVIEW_BYTES)
            }
            other => panic!("expected Unrecognised, got {other:?}"),
        }
    }

    #[test]
    fn an_oversize_image_reports_its_size_and_the_limit() {
        let temp = TempDir::new().unwrap();
        let path = write(temp.path(), "large.png", &[7u8; 100]);

        match prepare_with(&path, &policy(32)) {
            Err(ImageError::TooLarge { bytes, limit, .. }) => {
                assert_eq!(limit, 32);
                assert_eq!(bytes, 33, "one byte past the limit is how it is detected");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_file_exactly_at_the_limit_is_accepted() {
        // The read stops one byte past the limit so this case stays decidable
        // without reading the file whole.
        let temp = TempDir::new().unwrap();
        let mut bytes = png();
        bytes.extend(vec![0u8; 24]);
        let path = write(temp.path(), "exact.png", &bytes);

        let prepared = prepare_with(&path, &policy(32)).expect("a file at the limit is fine");
        assert_eq!(prepared.bytes, 32);
    }

    #[test]
    fn a_read_stops_at_the_limit_instead_of_reading_the_whole_file() {
        // A 10 MiB file against a 1 KiB policy. Reading it first to learn its
        // size would allocate all 10 MiB on the way to refusing it, which is the
        // case the bound exists for.
        let temp = TempDir::new().unwrap();
        let megabyte = vec![0u8; 1024 * 1024];
        let mut file = std::fs::File::create(temp.path().join("huge.png")).unwrap();
        use std::io::Write;
        for _ in 0..10 {
            file.write_all(&megabyte).unwrap();
        }
        drop(file);

        match prepare_with(&temp.path().join("huge.png"), &policy(1024)) {
            Err(ImageError::TooLarge { bytes, limit, .. }) => {
                assert_eq!(limit, 1024);
                assert_eq!(bytes, 1025);
                assert!(
                    bytes < 1024 * 1024,
                    "{} bytes were read against a 1024 byte limit",
                    bytes
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn the_mime_type_comes_from_the_bytes_and_not_the_filename() {
        // A `.png` that is really a JPEG. The extension is the only thing the
        // user chose, and the provider is what the type has to be right for.
        let temp = TempDir::new().unwrap();
        let path = write(temp.path(), "photo.png", &jpeg());

        let prepared = prepare(&path).expect("a JPEG named .png");
        assert_eq!(prepared.format, ImageFormat::Jpeg);
        assert_eq!(prepared.attachment.mime_type, "image/jpeg");
        assert_eq!(prepared.attachment.filename, "photo.png");
        assert!(prepared.attachment.is_image());
    }

    #[test]
    fn a_prepared_image_carries_its_own_bytes_and_length() {
        let temp = TempDir::new().unwrap();
        let path = write(temp.path(), "shot.png", &png());

        let prepared = prepare(&path).expect("a png");
        assert_eq!(prepared.bytes, 8);
        assert_eq!(prepared.attachment.data, png());
        assert_eq!(prepared.format.mime_type(), "image/png");
    }

    #[test]
    fn an_in_memory_image_is_prepared_the_same_way() {
        let prepared =
            prepare_bytes(jpeg(), "Screenshot 2026-01-01 at 10.00.00.png").expect("a pasted jpeg");
        assert_eq!(prepared.format, ImageFormat::Jpeg);
        assert_eq!(prepared.attachment.mime_type, "image/jpeg");
        assert_eq!(
            prepared.attachment.filename,
            "Screenshot 2026-01-01 at 10.00.00.png"
        );
        assert_eq!(prepared.bytes, jpeg().len());
    }

    #[test]
    fn an_in_memory_image_is_still_held_to_the_policy() {
        assert!(matches!(
            prepare_bytes_with(vec![0; 100], "a.png", &policy(32)),
            Err(ImageError::TooLarge { limit: 32, .. })
        ));
        assert!(matches!(
            prepare_bytes_with(b"".to_vec(), "a.png", &policy(32)),
            Err(ImageError::Empty { .. })
        ));
        assert!(matches!(
            prepare_bytes_with(text(), "a.txt", &policy(32)),
            Err(ImageError::Unrecognised { .. })
        ));
    }

    #[test]
    fn the_default_policy_is_five_mebibytes_and_enforces_no_dimension() {
        // Five MiB is above a full-resolution screenshot and below the size at
        // which a provider refuses the whole turn.
        assert_eq!(DEFAULT_MAX_IMAGE_BYTES, 5 * 1024 * 1024);
        let default = ImagePolicy::default();
        assert_eq!(default.max_bytes, DEFAULT_MAX_IMAGE_BYTES);
        assert_eq!(default.max_dimension, None);
        assert_eq!(
            ImagePolicy::new(10).max_dimension,
            None,
            "a dimension limit would be a check nothing performs"
        );
    }

    #[tokio::test]
    async fn an_image_is_prepared_under_the_same_policy_off_the_calling_thread() {
        let temp = TempDir::new().unwrap();
        let path = write(temp.path(), "shot.png", &png());

        let prepared = prepare_async(&path).await.expect("a png");
        assert_eq!(prepared.format, ImageFormat::Png);
        assert_eq!(prepared.attachment.mime_type, "image/png");
        assert_eq!(prepared.bytes, 8);
    }

    #[tokio::test]
    async fn an_async_failure_reports_the_path_it_was_asked_for() {
        let temp = TempDir::new().unwrap();
        let absent = temp.path().join("absent.png");

        match prepare_async(&absent).await {
            Err(ImageError::Unreadable { path, .. }) => assert_eq!(path, absent),
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_batch_reports_each_image_separately() {
        // One unreadable file must not cost the user the other two.
        let temp = TempDir::new().unwrap();
        let paths = vec![
            write(temp.path(), "one.png", &png()),
            temp.path().join("missing.png"),
            write(temp.path(), "two.jpeg", &jpeg()),
        ];

        let results = prepare_many(&paths, &ImagePolicy::default()).await;

        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(!results[1].is_ok());
        assert!(results[2].is_ok());
        assert_eq!(
            results[2].attachment().map(|a| a.mime_type.as_str()),
            Some("image/jpeg")
        );
        assert!(matches!(
            results[1].result,
            Err(ImageError::Unreadable { .. })
        ));
    }

    #[tokio::test]
    async fn a_batch_keeps_the_order_it_was_given() {
        // Completion order is the disk's; the caller's order is the one the user
        // chose, and the composer attaches by position.
        let temp = TempDir::new().unwrap();
        let paths: Vec<PathBuf> = (0..8)
            .map(|index| write(temp.path(), &format!("{index}.png"), &png()))
            .collect();

        let results = prepare_many(&paths, &ImagePolicy::default()).await;

        for (index, path) in paths.iter().enumerate() {
            assert_eq!(&results[index].path, path, "position {index} shifted");
        }
    }

    #[tokio::test]
    async fn a_batch_never_keeps_more_than_the_limit_in_flight() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let items: Vec<usize> = (0..MAX_CONCURRENT_IMAGE_READS * 3).collect();

        let results = map_bounded(items, {
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            move |item| {
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                async move {
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    // Enough of a yield for every admitted task to register
                    // before any of them finishes.
                    tokio::task::yield_now().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    item
                }
            }
        })
        .await;

        assert_eq!(results.len(), MAX_CONCURRENT_IMAGE_READS * 3);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            MAX_CONCURRENT_IMAGE_READS,
            "the batch did not run in parallel at all"
        );
        for (index, item) in &results {
            assert_eq!(index, item, "the batch reordered its own results");
        }
    }

    #[tokio::test]
    async fn a_batch_of_nothing_is_empty_rather_than_an_error() {
        assert!(prepare_many(&[], &ImagePolicy::default()).await.is_empty());
    }

    #[test]
    fn a_batch_result_hands_over_its_attachment_or_its_error() {
        let temp = TempDir::new().unwrap();
        let good = write(temp.path(), "one.png", &png());
        let bad = write(temp.path(), "two.png", &text());

        let results = vec![
            PreparedImageResult {
                path: good.clone(),
                result: prepare(&good),
            },
            PreparedImageResult {
                path: bad.clone(),
                result: prepare(&bad),
            },
        ];

        assert_eq!(results[0].attachment().map(|a| &a.data), Some(&png()));
        assert!(results[1].attachment().is_none());

        let mut results = results.into_iter();
        assert!(results.next().unwrap().into_result().is_ok());
        assert!(results.next().unwrap().into_result().is_err());
    }
}
