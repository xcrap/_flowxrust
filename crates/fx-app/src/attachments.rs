//! Image attachments: loading them for the transcript, and holding them in
//! the composer until they are sent.
//!
//! File reads and pixel decoding run in GPUI's asset executor; only the
//! downsampled first frame is retained for painting.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use fx_core::{
    Attachment, ConversationImageAssetReference, ConversationMessage, MessageContent, MessageRole,
};
use fx_design::components::attachment::{
    IMAGE_PREVIEW_MAX_WIDTH, attachment_chip, attachment_fallback, image_preview,
};
use fx_design::components::{IconButtonProps, icon_button};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{Colors, Radii, Spacing, global};
use fx_store::AppPaths;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Asset, ElementId, RenderImage, RenderOnce, SharedString, Window, div, px,
};
use image::{ImageDecoder, ImageReader};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const THUMBNAIL_WIDTH: u32 = 640;
const THUMBNAIL_HEIGHT: u32 = 440;

/// How many images' keys are remembered. A key is a few words, so this is
/// generous: it only has to outnumber the images one transcript paints.
const MAX_REMEMBERED_KEYS: usize = 4096;
/// How many shared byte copies are kept, and how large they may grow together.
const MAX_SHARED_IMAGES: usize = 256;
const MAX_SHARED_BYTES: usize = 64 * 1024 * 1024;
/// Bytes sampled from each end, and from the middle, to recognise a buffer.
const IDENTITY_WINDOW: usize = 64;

/// An inline image with its content key computed once.
///
/// `Hash` writes the key alone. GPUI hashes an asset's source on every
/// `use_asset`, which for the bytes themselves meant a full SipHash of every
/// screenshot on every repaint, on top of the one this cache already paid.
#[derive(Clone)]
pub(crate) struct InlineImage {
    key: u64,
    data: Arc<[u8]>,
}

impl Hash for InlineImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

/// Which buffer an inline image is, without reading all of it.
///
/// The transcript hands over a borrow of the same `Vec` on every repaint, so
/// its address and length identify it. The sampled ends are what keep a freed
/// buffer's address, reused for a different image of the same length, from
/// inheriting the old key: the last bytes of a PNG or a JPEG are checksums and
/// entropy-coded data, which two different images do not share.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Identity {
    address: usize,
    len: usize,
    sample: u64,
}

impl Identity {
    fn of(data: &[u8]) -> Self {
        let mut hasher = std::hash::DefaultHasher::new();
        let window = IDENTITY_WINDOW.min(data.len());
        let middle = data.len() / 2;
        data[..window].hash(&mut hasher);
        data[middle - middle.min(window / 2)..(middle + window / 2).min(data.len())]
            .hash(&mut hasher);
        data[data.len() - window..].hash(&mut hasher);
        Self {
            address: data.as_ptr() as usize,
            len: data.len(),
            sample: hasher.finish(),
        }
    }
}

struct Shared {
    data: Arc<[u8]>,
    used: u64,
}

// Repainting the transcript must neither hash nor copy each attached
// screenshot again. Keys are remembered per buffer, so a repaint reads a few
// hundred bytes rather than the image; shared byte copies are kept per key and
// evicted least-recently-used. Decoded thumbnails live in GPUI's asset cache.
#[derive(Default)]
struct InlineCache {
    keys: HashMap<Identity, u64>,
    key_order: VecDeque<Identity>,
    shared: HashMap<u64, Shared>,
    bytes: usize,
    clock: u64,
    /// Full-content hashes taken, which a test reads to prove a repaint takes none.
    digests: usize,
}

impl InlineCache {
    fn intern(&mut self, data: &[u8]) -> InlineImage {
        self.clock += 1;
        let identity = Identity::of(data);
        let key = match self.keys.get(&identity) {
            Some(&key) => key,
            None => {
                let key = self.key_for(data);
                if self.key_order.len() >= MAX_REMEMBERED_KEYS
                    && let Some(oldest) = self.key_order.pop_front()
                {
                    self.keys.remove(&oldest);
                }
                self.keys.insert(identity, key);
                self.key_order.push_back(identity);
                key
            }
        };
        if let Some(shared) = self.shared.get_mut(&key) {
            shared.used = self.clock;
            return InlineImage {
                key,
                data: shared.data.clone(),
            };
        }
        while !self.shared.is_empty()
            && (self.shared.len() >= MAX_SHARED_IMAGES
                || self.bytes + data.len() > MAX_SHARED_BYTES)
        {
            let Some(oldest) = self
                .shared
                .iter()
                .min_by_key(|(_, shared)| shared.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(evicted) = self.shared.remove(&oldest) {
                self.bytes -= evicted.data.len();
            }
        }
        let data: Arc<[u8]> = data.into();
        self.bytes += data.len();
        self.shared.insert(
            key,
            Shared {
                data: data.clone(),
                used: self.clock,
            },
        );
        InlineImage { key, data }
    }

    /// The content key for bytes this cache has not seen as a buffer before.
    ///
    /// Two different images landing on one 64-bit key would show one in place
    /// of the other, so a key already holding different bytes is stepped past.
    /// This runs once per buffer, which is the only place a full compare is
    /// affordable.
    fn key_for(&mut self, data: &[u8]) -> u64 {
        self.digests += 1;
        let mut hasher = std::hash::DefaultHasher::new();
        data.hash(&mut hasher);
        let mut key = hasher.finish();
        while self
            .shared
            .get(&key)
            .is_some_and(|shared| shared.data.as_ref() != data)
        {
            key = key.wrapping_add(1);
        }
        key
    }
}

thread_local! {
    static INLINE_CACHE: RefCell<InlineCache> = RefCell::new(InlineCache::default());
}

#[derive(Clone, Hash)]
pub(crate) enum ImageInput {
    Inline(InlineImage),
    Mentioned(PathBuf),
    Asset {
        root: PathBuf,
        reference: ConversationImageAssetReference,
    },
    Unavailable,
}

impl ImageInput {
    pub(crate) fn inline(data: &[u8]) -> Self {
        if data.len() > MAX_IMAGE_BYTES {
            Self::Unavailable
        } else {
            Self::Inline(INLINE_CACHE.with(|cache| cache.borrow_mut().intern(data)))
        }
    }

    pub(crate) fn asset(
        paths: Option<&AppPaths>,
        reference: &ConversationImageAssetReference,
    ) -> Self {
        match paths {
            Some(paths) => Self::Asset {
                root: paths.root().to_path_buf(),
                reference: reference.clone(),
            },
            None => Self::Unavailable,
        }
    }
}

#[derive(Clone)]
struct DecodedPreview {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

struct PreviewLoader;

impl Asset for PreviewLoader {
    type Source = ImageInput;
    type Output = Result<DecodedPreview, &'static str>;

    // An async fn captures &mut App, which is !Send; this background future
    // must own only the source and never retain the UI context.
    #[allow(clippy::manual_async_fn)]
    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { load_preview(&source) }
    }
}

fn source_bytes(source: &ImageInput) -> Result<Arc<[u8]>, &'static str> {
    let data: Arc<[u8]> = match source {
        ImageInput::Inline(image) => image.data.clone(),
        ImageInput::Mentioned(path) => {
            if !path.is_absolute() {
                return Err("Image unavailable");
            }
            let path = path.canonicalize().map_err(|_| "Image unavailable")?;
            read_bounded(&path)?.into()
        }
        ImageInput::Asset { root, reference } => {
            let path = resolve_asset(root, reference).ok_or("Image unavailable")?;
            read_bounded(&path)?.into()
        }
        ImageInput::Unavailable => return Err("Image unavailable"),
    };
    Ok(data)
}

/// An attachment's original bytes, ready for the clipboard.
///
/// The original rather than the preview: a thumbnail pasted elsewhere would
/// silently be a smaller image than the one that was sent.
pub(crate) fn clipboard_image(source: &ImageInput) -> Result<gpui::ClipboardItem, &'static str> {
    let data = source_bytes(source)?;
    let format = fx_core::services::images::sniff(&data).ok_or("Image format is not supported")?;
    let format = gpui::ImageFormat::from_mime_type(format.mime_type())
        .ok_or("Image format is not supported")?;
    Ok(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
        format,
        data.to_vec(),
    )))
}

fn load_preview(source: &ImageInput) -> Result<DecodedPreview, &'static str> {
    decode_preview(&source_bytes(source)?)
}

fn resolve_asset(root: &Path, reference: &ConversationImageAssetReference) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let directory = AppPaths::at(&root)
        .assets_dir(reference.project_id, reference.agent_id)
        .canonicalize()
        .ok()?;
    if !directory.starts_with(&root) {
        return None;
    }
    let path = directory.join(reference.filename()).canonicalize().ok()?;
    path.starts_with(directory).then_some(path)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "Image unavailable")?;
    let metadata = file.metadata().map_err(|_| "Image unavailable")?;
    if !metadata.is_file() {
        return Err("Image unavailable");
    }
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err("Image is too large to preview");
    }
    let mut data = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Image unavailable")?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err("Image is too large to preview");
    }
    Ok(data)
}

fn decode_preview(data: &[u8]) -> Result<DecodedPreview, &'static str> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err("Image is too large to preview");
    }
    fx_core::services::images::sniff(data).ok_or("Image format is not supported")?;
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| "Image could not be decoded")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| "Image could not be decoded")?;
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err("Image is too large to preview");
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded =
        image::DynamicImage::from_decoder(decoder).map_err(|_| "Image could not be decoded")?;
    decoded.apply_orientation(orientation);
    let (width, height) = (decoded.width(), decoded.height());
    let mut pixels = decoded
        .thumbnail(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT)
        .into_rgba8();
    // GPUI's atlas expects BGRA, while image decoders produce RGBA.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(DecodedPreview {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        width,
        height,
    })
}

/// Native viewers need a real image suffix. Create a private original-resolution
/// copy only after a click, and keep it alive in the app's asset cache until quit.
struct OpenImageLoader;

impl Asset for OpenImageLoader {
    type Source = ImageInput;
    type Output = Result<Arc<tempfile::TempPath>, &'static str>;

    // Keep the !Send App argument out of the 'static background future.
    #[allow(clippy::manual_async_fn)]
    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { native_copy(&source) }
    }
}

fn native_copy(source: &ImageInput) -> Result<Arc<tempfile::TempPath>, &'static str> {
    let data = source_bytes(source)?;
    let format = fx_core::services::images::sniff(&data).ok_or("Image format is not supported")?;
    let mut file = tempfile::Builder::new()
        .prefix("flowx-image-")
        .suffix(&format!(".{}", format.extension()))
        .tempfile()
        .map_err(|_| "Image could not be opened")?;
    file.write_all(&data)
        .map_err(|_| "Image could not be opened")?;
    file.flush().map_err(|_| "Image could not be opened")?;
    Ok(Arc::new(file.into_temp_path()))
}

#[derive(IntoElement)]
pub(crate) struct AttachmentPreview {
    id: ElementId,
    input: ImageInput,
    label: SharedString,
}

pub(crate) fn preview(
    id: impl Into<ElementId>,
    input: ImageInput,
    label: impl Into<SharedString>,
) -> AttachmentPreview {
    AttachmentPreview {
        id: id.into(),
        input,
        label: label.into(),
    }
}

impl RenderOnce for AttachmentPreview {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = global::current();
        let loaded = window.use_asset::<PreviewLoader>(&self.input, cx);
        let (body, can_open): (AnyElement, _) = match loaded {
            Some(Ok(preview)) => (
                image_preview(preview.image, preview.width, preview.height),
                true,
            ),
            Some(Err(error)) => (attachment_fallback(self.label.clone(), error), false),
            None => (
                attachment_fallback(self.label.clone(), "Loading preview…"),
                false,
            ),
        };
        let input = self.input;
        div()
            .id(self.id)
            .w(px(IMAGE_PREVIEW_MAX_WIDTH))
            .min_w_0()
            .max_w_full()
            .flex()
            .flex_col()
            .items_start()
            .gap(px_len(Spacing::XS))
            .rounded(px_len(Radii::MD))
            .when(can_open, |this| {
                this.cursor_pointer()
                    .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
                    .on_click(move |_, window, cx| {
                        let (file, _) = cx.fetch_asset::<OpenImageLoader>(&input);
                        window
                            .spawn(cx, async move |cx| {
                                if let Ok(path) = file.await {
                                    let _ = cx.update(|_, cx| {
                                        cx.open_with_system(path.as_ref().as_ref())
                                    });
                                }
                            })
                            .detach();
                    })
            })
            .child(body)
            .child(
                div()
                    .debug_selector(|| "attachment.caption".into())
                    .w_full()
                    .truncate()
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child(self.label),
            )
    }
}

// ── Composer attachments ──────────────────────────────────────────────────

/// How many images may wait in one composer, and how large they may be
/// together. The original's limits.
pub(crate) const MAX_PENDING_ATTACHMENTS: usize = 10;
pub(crate) const MAX_PENDING_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// The user's row for a prompt: its images first, then its words, as the
/// original wrote it.
pub(crate) fn user_message(text: &str, attachments: &[Attachment]) -> ConversationMessage {
    let mut content: Vec<MessageContent> = attachments
        .iter()
        .filter(|attachment| attachment.is_image())
        .map(|attachment| MessageContent::Image {
            data: attachment.data.clone(),
            mime_type: attachment.mime_type.clone(),
        })
        .collect();
    content.push(MessageContent::Text { text: text.into() });
    ConversationMessage::new(MessageRole::User, content)
}

/// The images a user row carries, as attachments again.
///
/// This is how an unsent prompt keeps its images through a retry and a
/// restart: the row it left in the transcript already holds the bytes, so
/// recovery reads them back rather than storing a second copy beside it.
pub(crate) fn attachments_in(message: &ConversationMessage) -> Vec<Attachment> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::Image { data, mime_type } => Some((data, mime_type)),
            _ => None,
        })
        .enumerate()
        .map(|(index, (data, mime_type))| {
            let extension = fx_core::services::images::sniff(data)
                .map(|format| format.extension())
                .unwrap_or("png");
            Attachment::new(
                data.clone(),
                mime_type.clone(),
                format!("Image {}.{extension}", index + 1),
            )
        })
        .collect()
}

/// What preparing a batch of images produced: what can attach, and why the
/// rest cannot.
#[derive(Debug, Default)]
pub struct Prepared {
    pub attachments: Vec<Attachment>,
    pub errors: Vec<String>,
}

/// Read images the reader picked or dropped. Blocking IO: call it off the
/// main thread.
pub(crate) fn prepare_paths(paths: &[PathBuf]) -> Prepared {
    let mut prepared = Prepared::default();
    for path in paths {
        match fx_core::services::images::prepare(path) {
            Ok(image) => prepared.attachments.push(image.attachment),
            Err(error) => prepared.errors.push(describe_image_error(path, &error)),
        }
    }
    prepared
}

/// Prepare a pasted image. Only formats the providers accept get through:
/// a TIFF on the clipboard has no PNG beside it to fall back on.
pub(crate) fn prepare_pasted(image: &gpui::Image) -> Result<Attachment, String> {
    let extension = match image.format {
        gpui::ImageFormat::Jpeg => "jpg",
        gpui::ImageFormat::Gif => "gif",
        gpui::ImageFormat::Webp => "webp",
        _ => "png",
    };
    fx_core::services::images::prepare_bytes(
        image.bytes.clone(),
        &format!("Pasted Image.{extension}"),
    )
    .map(|prepared| prepared.attachment)
    .map_err(|error| describe_pasted_error(&error))
}

/// Why a pasted image cannot attach. A paste over the size limit is still an
/// image; calling it "not an image" sent the reader looking for
/// the wrong problem.
fn describe_pasted_error(error: &fx_core::services::images::ImageError) -> String {
    use fx_core::services::images::ImageError;
    match error {
        ImageError::TooLarge { limit, .. } => format!(
            "The pasted image is larger than {} MB.",
            limit / (1024 * 1024)
        ),
        ImageError::Empty { .. } => "The pasted image is empty.".into(),
        _ => "The clipboard does not contain a supported image.".into(),
    }
}

fn describe_image_error(path: &Path, error: &fx_core::services::images::ImageError) -> String {
    use fx_core::services::images::ImageError;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match error {
        ImageError::TooLarge { limit, .. } => {
            format!("{name} is larger than {} MB.", limit / (1024 * 1024))
        }
        ImageError::Unrecognised { .. } | ImageError::NotAFile { .. } => {
            format!("{name} is not a supported image.")
        }
        ImageError::Empty { .. } => format!("{name} is empty."),
        ImageError::Unreadable { .. } => format!("{name} could not be read."),
    }
}

/// Add prepared images under the composer's limits, returning what to say
/// about anything left out.
pub(crate) fn admit(pending: &mut Vec<Attachment>, prepared: Prepared) -> Option<String> {
    let mut errors = prepared.errors;
    let mut bytes: usize = pending.iter().map(|attachment| attachment.data.len()).sum();
    for attachment in prepared.attachments {
        if pending.len() >= MAX_PENDING_ATTACHMENTS {
            errors.push(format!(
                "Only {MAX_PENDING_ATTACHMENTS} images can be attached at once."
            ));
            break;
        }
        if bytes + attachment.data.len() > MAX_PENDING_ATTACHMENT_BYTES {
            errors.push("Attachments cannot exceed 50 MB in total.".into());
            break;
        }
        bytes += attachment.data.len();
        pending.push(attachment);
    }
    error_summary(errors)
}

/// At most three distinct reasons, then a count of the rest.
fn error_summary(errors: Vec<String>) -> Option<String> {
    let mut unique: Vec<String> = errors;
    unique.sort();
    unique.dedup();
    if unique.is_empty() {
        return None;
    }
    let hidden = unique.len().saturating_sub(3);
    let mut summary = unique.into_iter().take(3).collect::<Vec<_>>().join(" ");
    if hidden > 0 {
        summary.push_str(&format!(" {hidden} more file(s) were skipped."));
    }
    Some(summary)
}

/// What a pending image's remove control calls.
pub(crate) type RemoveHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One image waiting in the composer, with its remove control.
#[derive(IntoElement)]
pub(crate) struct PendingChip {
    input: ImageInput,
    remove_label: SharedString,
    on_remove: RemoveHandler,
}

pub(crate) fn pending_chip(
    input: ImageInput,
    remove_label: impl Into<SharedString>,
    on_remove: RemoveHandler,
) -> PendingChip {
    PendingChip {
        input,
        remove_label: remove_label.into(),
        on_remove,
    }
}

impl RenderOnce for PendingChip {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let image = window
            .use_asset::<PreviewLoader>(&self.input, cx)
            .and_then(Result::ok)
            .map(|preview| preview.image);
        let on_remove = self.on_remove;
        attachment_chip(
            image,
            icon_button(
                IconButtonProps::new(Icon::XMARK, self.remove_label)
                    .tiny()
                    .ghost(),
                move |_, window, cx| on_remove(window, cx),
            )
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_over_the_limit_says_it_is_too_large() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(fx_core::services::images::DEFAULT_MAX_IMAGE_BYTES + 1, 0);
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
        assert_eq!(
            prepare_pasted(&image).unwrap_err(),
            "The pasted image is larger than 5 MB."
        );
        let text = gpui::Image::from_bytes(gpui::ImageFormat::Png, b"plain words".to_vec());
        assert_eq!(
            prepare_pasted(&text).unwrap_err(),
            "The clipboard does not contain a supported image."
        );
    }
    use image::{ImageBuffer, ImageFormat, Rgba};
    use uuid::Uuid;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_pixel(width, height, Rgba([255u8, 0, 0, 255]));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png).unwrap();
        output.into_inner()
    }

    #[test]
    fn real_pixels_are_decoded_downsampled_and_converted_for_gpui() {
        let preview = decode_preview(&png(1858, 2488)).unwrap();
        assert_eq!((preview.width, preview.height), (1858, 2488));
        assert!(preview.image.size(0).width.0 <= THUMBNAIL_WIDTH as i32);
        assert!(preview.image.size(0).height.0 <= THUMBNAIL_HEIGHT as i32);
        assert_eq!(&preview.image.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn missing_corrupt_and_oversized_images_fail_without_a_thumbnail() {
        assert!(
            load_preview(&ImageInput::Mentioned(PathBuf::from("/does-not-exist.png"))).is_err()
        );
        assert!(decode_preview(b"not a png").is_err());
        assert!(decode_preview(&vec![0; MAX_IMAGE_BYTES + 1]).is_err());
    }

    #[test]
    fn native_open_keeps_full_resolution_bytes_and_uses_a_private_image_file() {
        let original = png(400, 200);
        let copy = native_copy(&ImageInput::inline(&original)).unwrap();
        assert_eq!(copy.extension().unwrap(), "png");
        assert_eq!(std::fs::read(copy.as_ref()).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(copy.as_ref())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn repainting_an_inline_image_hashes_its_bytes_once() {
        let mut cache = InlineCache::default();
        let image = png(400, 300);
        let first = cache.intern(&image);
        for _ in 0..100 {
            let again = cache.intern(&image);
            assert!(Arc::ptr_eq(&first.data, &again.data));
            assert_eq!(first.key, again.key);
        }
        assert_eq!(cache.digests, 1, "a repaint re-hashed the whole image");
    }

    #[test]
    fn more_images_than_the_old_sixteen_neither_rehash_nor_evict_each_other() {
        // The previous cache held sixteen entries, so a transcript with a
        // seventeenth screenshot evicted, re-hashed and re-copied every image
        // on every repaint.
        let images: Vec<Vec<u8>> = (0..64).map(|index| png(8 + index, 8)).collect();
        let mut cache = InlineCache::default();
        let first: Vec<InlineImage> = images.iter().map(|image| cache.intern(image)).collect();
        for _ in 0..3 {
            for (image, earlier) in images.iter().zip(&first) {
                assert!(Arc::ptr_eq(&earlier.data, &cache.intern(image).data));
            }
        }
        assert_eq!(cache.digests, images.len());
        assert_eq!(cache.shared.len(), images.len());
    }

    #[test]
    fn the_asset_key_ignores_the_bytes_and_still_separates_images() {
        struct Counting(usize);
        impl Hasher for Counting {
            fn finish(&self) -> u64 {
                0
            }
            fn write(&mut self, bytes: &[u8]) {
                self.0 += bytes.len();
            }
        }
        let mut cache = InlineCache::default();
        let large = png(1200, 900);
        let input = ImageInput::Inline(cache.intern(&large));
        let mut hasher = Counting(0);
        input.hash(&mut hasher);
        assert!(
            hasher.0 < 64,
            "GPUI hashed {} bytes of source per repaint",
            hasher.0
        );
        let other = cache.intern(&png(1200, 901));
        assert_ne!(cache.intern(&large).key, other.key);
        // Same bytes in a different buffer resolve to the same key, so a
        // transcript reloaded from disk reuses the decoded preview.
        let copy = large.clone();
        assert_eq!(cache.intern(&copy).key, cache.intern(&large).key);
    }

    #[test]
    fn shared_copies_stay_within_their_byte_budget() {
        let mut cache = InlineCache::default();
        let big = vec![7u8; MAX_SHARED_BYTES / 3 + 1];
        for index in 0..4u8 {
            let mut image = big.clone();
            image[0] = index;
            cache.intern(&image);
        }
        assert!(cache.bytes <= MAX_SHARED_BYTES);
        assert_eq!(cache.shared.len(), 2);
    }

    #[test]
    fn narrow_attachment_cards_keep_legible_caption_and_compact_fallback_geometry() {
        use gpui::{Context, Render, TestAppContext, VisualTestContext, size};
        use std::ops::Deref;

        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w_full().flex().flex_col().items_end().child(preview(
                    "test-image",
                    ImageInput::Unavailable,
                    "Screenshot 2026-10-13 at 00.59.11.png",
                ))
            }
        }

        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Probe);
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(180.0), px(400.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let caption = visual.debug_bounds("attachment.caption").unwrap();
        let fallback = visual.debug_bounds("attachment.fallback").unwrap();
        assert_eq!(caption.size.width, px(180.0));
        assert!(caption.size.height < px(30.0));
        assert_eq!(fallback.size.width, px(180.0));
        assert!(fallback.size.height < px(70.0));
    }

    #[test]
    fn stored_assets_resolve_only_within_their_own_asset_directory() {
        let temp = tempfile::tempdir().unwrap();
        let reference = ConversationImageAssetReference {
            project_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
            content_index: 0,
            mime_type: "image/png".into(),
            byte_count: 0,
        };
        let paths = AppPaths::at(temp.path().join("state"));
        let directory = paths.assets_dir(reference.project_id, reference.agent_id);
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join(reference.filename());
        std::fs::write(&file, png(20, 10)).unwrap();
        assert!(load_preview(&ImageInput::asset(Some(&paths), &reference)).is_ok());
        std::fs::remove_file(&file).unwrap();
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside.png");
            std::fs::write(&outside, png(20, 10)).unwrap();
            std::os::unix::fs::symlink(outside, &file).unwrap();
            assert!(load_preview(&ImageInput::asset(Some(&paths), &reference)).is_err());
        }
    }
}
