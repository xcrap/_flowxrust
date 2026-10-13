//! Transcript image loading. File reads and pixel decoding run in GPUI's asset
//! executor; only the downsampled first frame is retained for painting.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fx_core::ConversationImageAssetReference;
use fx_design::components::attachment::{
    IMAGE_PREVIEW_MAX_WIDTH, attachment_fallback, image_preview,
};
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

// Repainting the transcript must not copy each attached screenshot again.
// Keep a small LRU of shared source bytes; decoded thumbnails use GPUI's cache.
#[derive(Default)]
struct InlineCache {
    items: VecDeque<(u64, Arc<[u8]>)>,
    bytes: usize,
}

impl InlineCache {
    fn intern(&mut self, data: &[u8]) -> Arc<[u8]> {
        let mut hasher = std::hash::DefaultHasher::new();
        data.hash(&mut hasher);
        let digest = hasher.finish();
        if let Some(index) = self
            .items
            .iter()
            .position(|(key, stored)| *key == digest && stored.as_ref() == data)
        {
            let item = self.items.remove(index).expect("located cached image");
            let result = item.1.clone();
            self.items.push_back(item);
            return result;
        }
        while self.items.len() >= 16 || self.bytes + data.len() > 32 * 1024 * 1024 {
            let Some((_, oldest)) = self.items.pop_front() else {
                break;
            };
            self.bytes -= oldest.len();
        }
        let data: Arc<[u8]> = data.into();
        self.bytes += data.len();
        self.items.push_back((digest, data.clone()));
        data
    }
}

thread_local! {
    static INLINE_CACHE: RefCell<InlineCache> = RefCell::new(InlineCache::default());
}

#[derive(Clone, Hash)]
pub(crate) enum ImageInput {
    Inline(Arc<[u8]>),
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
        ImageInput::Inline(data) => data.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn repeated_rendering_reuses_inline_bytes_and_the_cache_evicts_old_sources() {
        let mut cache = InlineCache::default();
        let first = cache.intern(b"first image");
        assert!(Arc::ptr_eq(&first, &cache.intern(b"first image")));
        for index in 0..32 {
            cache.intern(&[index]);
        }
        assert_eq!(cache.items.len(), 16);
        assert!(!Arc::ptr_eq(&first, &cache.intern(b"first image")));
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
