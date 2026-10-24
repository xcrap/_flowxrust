//! Markdown prose rendered by GPUI, without HTML or a browser surface.
//!
//! Parsing keeps byte ranges against the displayed text, not against Markdown
//! source. GPUI can then wrap and hit-test an entire paragraph, including links
//! spanning several lines, without a separate layout box for every span.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use fx_design::components::rich_text::{InlineSpan, InlineStyle, rich_text};
use fx_design::components::{IconButtonProps, divider, icon_button, panel, tooltip};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{AnyElement, App, ClipboardItem, InteractiveText, SharedString, div};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use url::Url;

use crate::selection::{Paragraph, SelectableText};

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkTarget {
    Web(String),
    Local(PathBuf),
}

impl LinkTarget {
    fn parse(destination: &str) -> Option<Self> {
        if destination.chars().any(char::is_control) {
            return None;
        }
        if destination.starts_with('/') && !destination.starts_with("//") {
            // Markdown paths are URL-escaped, e.g. My%20Project/main.rs:12.
            let url = Url::parse(&format!("file://{destination}")).ok()?;
            let path = url.to_file_path().ok()?;
            let text = path.to_str()?;
            if text.chars().any(char::is_control) {
                return None;
            }
            // A line reference is useful in the label but Finder needs the
            // actual file. Do not mistake an ordinary colon in a name for one.
            let path = text.rsplit_once(':').map_or(text, |(path, line)| {
                if !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()) {
                    path
                } else {
                    text
                }
            });
            return Some(Self::Local(PathBuf::from(path)));
        }
        let url = Url::parse(destination).ok()?;
        if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
            Some(Self::Web(url.into()))
        } else {
            None
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Web(url) => url.clone(),
            Self::Local(path) => path.display().to_string(),
        }
    }

    fn open(&self, cx: &mut App) {
        match self {
            Self::Web(url) => cx.open_url(url),
            Self::Local(path) => cx.reveal_path(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    range: Range<usize>,
    target: LinkTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum BlockKind {
    #[default]
    Paragraph,
    Heading(u8),
    Code(String),
    Rule,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Block {
    kind: BlockKind,
    text: String,
    spans: Vec<InlineSpan>,
    links: Vec<Link>,
    marker: Option<String>,
    list_depth: usize,
    quote_depth: usize,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    current: Block,
    strong: usize,
    emphasis: usize,
    strikethrough: usize,
    active_links: Vec<Option<LinkTarget>>,
    lists: Vec<Option<u64>>,
    marker: Option<String>,
    quote_depth: usize,
}

impl Builder {
    fn flush(&mut self) {
        // A fence's body arrives with the newline before the closing fence.
        // Kept, it paints an empty last line and is copied as one.
        if matches!(self.current.kind, BlockKind::Code(_)) && self.current.text.ends_with('\n') {
            self.current.text.pop();
            let end = self.current.text.len();
            for span in &mut self.current.spans {
                span.range.end = span.range.end.min(end);
            }
            self.current.spans.retain(|span| !span.range.is_empty());
        }
        if !self.current.text.is_empty()
            || matches!(self.current.kind, BlockKind::Rule | BlockKind::Code(_))
        {
            self.blocks.push(std::mem::take(&mut self.current));
        }
        self.current = Block::default();
    }

    fn append(&mut self, text: &str, code: bool) {
        if text.is_empty() {
            return;
        }
        if self.current.text.is_empty() {
            self.current.marker = self.marker.take();
            self.current.list_depth = self.lists.len();
            self.current.quote_depth = self.quote_depth;
        }
        let start = self.current.text.len();
        self.current.text.push_str(text);
        let end = self.current.text.len();
        let target = self.active_links.last().and_then(Option::as_ref);
        let style = InlineStyle {
            strong: self.strong > 0,
            emphasis: self.emphasis > 0,
            strikethrough: self.strikethrough > 0,
            code,
            link: target.is_some(),
        };
        if let Some(last) = self
            .current
            .spans
            .last_mut()
            .filter(|last| last.style == style)
        {
            last.range.end = end;
        } else {
            self.current.spans.push(InlineSpan {
                range: start..end,
                style,
            });
        }
        if let Some(target) = target {
            if let Some(last) = self
                .current
                .links
                .last_mut()
                .filter(|last| last.range.end == start && &last.target == target)
            {
                last.range.end = end;
            } else {
                self.current.links.push(Link {
                    range: start..end,
                    target: target.clone(),
                });
            }
        }
    }
}

fn parse(source: &str) -> Vec<Block> {
    let mut builder = Builder::default();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::Paragraph | Tag::HtmlBlock) => builder.flush(),
            Event::Start(Tag::Heading { level, .. }) => {
                builder.flush();
                builder.current.kind = BlockKind::Heading(level as u8);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                builder.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                builder.current.kind = BlockKind::Code(language);
            }
            Event::Start(Tag::List(start)) => {
                builder.flush();
                builder.lists.push(start);
            }
            Event::Start(Tag::Item) => {
                builder.flush();
                builder.marker = Some(match builder.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}.");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "•".into(),
                });
            }
            Event::Start(Tag::BlockQuote(_)) => {
                builder.flush();
                builder.quote_depth += 1;
            }
            Event::Start(Tag::Strong) => builder.strong += 1,
            Event::Start(Tag::Emphasis) => builder.emphasis += 1,
            Event::Start(Tag::Strikethrough) => builder.strikethrough += 1,
            Event::Start(Tag::Link { dest_url, .. }) => {
                builder.active_links.push(LinkTarget::parse(&dest_url))
            }
            // Render image alt text as a safe link; transcript image blocks are
            // handled separately and remote Markdown images are never fetched.
            Event::Start(Tag::Image { dest_url, .. }) => {
                builder.active_links.push(LinkTarget::parse(&dest_url))
            }
            Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::HtmlBlock,
            ) => builder.flush(),
            Event::End(TagEnd::List(_)) => {
                builder.flush();
                builder.lists.pop();
            }
            Event::End(TagEnd::Item) => {
                builder.flush();
                builder.marker = None;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                builder.flush();
                builder.quote_depth = builder.quote_depth.saturating_sub(1);
            }
            Event::End(TagEnd::Strong) => builder.strong = builder.strong.saturating_sub(1),
            Event::End(TagEnd::Emphasis) => builder.emphasis = builder.emphasis.saturating_sub(1),
            Event::End(TagEnd::Strikethrough) => {
                builder.strikethrough = builder.strikethrough.saturating_sub(1)
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                builder.active_links.pop();
            }
            Event::Text(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::FootnoteReference(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => builder.append(&text, false),
            Event::Code(text) => builder.append(&text, true),
            Event::SoftBreak => builder.append(" ", false),
            Event::HardBreak => builder.append("\n", false),
            Event::TaskListMarker(checked) => {
                builder.marker = Some(if checked { "☑" } else { "☐" }.into())
            }
            Event::Rule => {
                builder.flush();
                builder.current.kind = BlockKind::Rule;
                builder.flush();
            }
            Event::Start(_) | Event::End(_) => {}
        }
    }
    builder.flush();
    builder.blocks
}

/// Parsed blocks for one content block, kept while it is still being shown.
struct CachedMarkdown {
    source: String,
    blocks: Rc<Vec<Block>>,
    /// The render pass that last used this entry.
    pass: u64,
}

/// Parsed Markdown, keyed by content-block id.
///
/// Bounded by the bytes of source it holds rather than by a count of entries.
/// A count was what this replaced, and it failed in the one shape that matters:
/// the transcript then rendered every block on every pass, so a conversation with
/// more blocks than the cap evicted each one just before the next pass needed
/// it, and every caret blink re-parsed the whole conversation.
///
/// Eviction is scan-resistant for the same reason. An entry the current pass
/// has already used is never evicted to make room for another entry of that
/// pass; once only those are left, a new block is parsed without being kept. A
/// conversation too large for the budget then re-parses only its overflow
/// instead of everything.
#[derive(Default)]
struct ParseCache {
    entries: HashMap<SharedString, CachedMarkdown>,
    bytes: usize,
    pass: u64,
    #[cfg(test)]
    parses: usize,
}

/// Source bytes the cache may hold across every visited conversation.
const MAX_CACHED_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// A block larger than this is parsed on each pass rather than crowding out
/// every other block in the cache.
const MAX_ENTRY_SOURCE_BYTES: usize = 1_048_576;

impl ParseCache {
    fn get(&mut self, id: &SharedString, source: &str) -> Rc<Vec<Block>> {
        let pass = self.pass;
        if let Some(entry) = self.entries.get_mut(id)
            && entry.source == source
        {
            entry.pass = pass;
            return entry.blocks.clone();
        }
        // A streamed block changes on every delta under the same id; its stale
        // parse is no use to anyone.
        if let Some(stale) = self.entries.remove(id) {
            self.bytes -= stale.source.len();
        }
        #[cfg(test)]
        {
            self.parses += 1;
        }
        let blocks = Rc::new(parse(source));
        if source.len() <= MAX_ENTRY_SOURCE_BYTES && self.make_room(source.len()) {
            self.bytes += source.len();
            self.entries.insert(
                id.clone(),
                CachedMarkdown {
                    source: source.to_owned(),
                    blocks: blocks.clone(),
                    pass,
                },
            );
        }
        blocks
    }

    /// Evict entries only earlier passes used, oldest first, until `incoming`
    /// fits. Returns whether it does.
    fn make_room(&mut self, incoming: usize) -> bool {
        if self.bytes + incoming <= MAX_CACHED_SOURCE_BYTES {
            return true;
        }
        let mut stale: Vec<(u64, SharedString)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.pass < self.pass)
            .map(|(id, entry)| (entry.pass, id.clone()))
            .collect();
        stale.sort_unstable_by_key(|(pass, _)| *pass);
        for (_, id) in stale {
            if let Some(entry) = self.entries.remove(&id) {
                self.bytes -= entry.source.len();
            }
            if self.bytes + incoming <= MAX_CACHED_SOURCE_BYTES {
                return true;
            }
        }
        false
    }
}

thread_local! {
    // Transcript refreshes also happen for caret and activity animation. Keep
    // unchanged paragraphs parsed, with bounds across all visited chats.
    static PARSE_CACHE: RefCell<ParseCache> = RefCell::new(ParseCache::default());
}

/// Start a render pass of the transcript.
///
/// Everything rendered after this belongs to the new pass, which is how the
/// cache tells blocks still on screen from blocks of a thread the reader left.
pub(crate) fn begin_pass() {
    PARSE_CACHE.with(|cache| cache.borrow_mut().pass += 1);
}

fn cached_parse(source: &str, id: &SharedString) -> Rc<Vec<Block>> {
    PARSE_CACHE.with(|cache| cache.borrow_mut().get(id, source))
}

/// Render one Markdown content block. `id` must distinguish it from other
/// content blocks in the same transcript and stay stable while streaming.
///
/// Each paragraph is selectable under the key `{id}:{index}`, so a caller that
/// starts `id` with a message id can ask whether a selection touches that
/// message.
pub(crate) fn render(source: &str, id: impl Into<SharedString>) -> AnyElement {
    let id = id.into();
    let blocks = cached_parse(source, &id);
    div()
        .id(id.clone())
        .min_w_0()
        .w_full()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .children(blocks.iter().enumerate().map(|(index, block)| {
            render_block(block, paragraph_key(&id, index), is_tight(&blocks, index))
        }))
        .into_any_element()
}

/// The selectable paragraphs [`render`] would draw for `source`, without
/// drawing any of them.
///
/// Same parse, same keys, same text: a selection that runs into a block that
/// is not on screen copies it from here.
pub(crate) fn paragraphs(source: &str, id: impl Into<SharedString>) -> Vec<Paragraph> {
    let id = id.into();
    let blocks = cached_parse(source, &id);
    blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Rule)
        .map(|(index, block)| Paragraph {
            key: paragraph_key(&id, index),
            text: SharedString::from(block.text.clone()),
            tight: is_tight(&blocks, index),
        })
        .collect()
}

fn paragraph_key(id: &SharedString, index: usize) -> SharedString {
    SharedString::from(format!("{id}:{index}"))
}

/// Consecutive list items are lines of one list rather than paragraphs, and
/// are copied back out that way.
fn is_tight(blocks: &[Block], index: usize) -> bool {
    index > 0 && blocks[index].list_depth > 0 && blocks[index - 1].list_depth > 0
}

fn render_block(block: &Block, id: SharedString, tight: bool) -> AnyElement {
    let theme = global::current();
    if block.kind == BlockKind::Rule {
        return divider().into_any_element();
    }
    let role = match block.kind {
        BlockKind::Heading(1) => TextRole::Title2,
        BlockKind::Heading(2 | 3) => TextRole::Title3,
        BlockKind::Heading(_) => TextRole::BodyMedium,
        BlockKind::Code(_) => TextRole::Mono,
        _ => TextRole::Body,
    };
    let shown = SharedString::from(block.text.clone());
    let styled = rich_text(shown.clone(), &block.spans, role, &theme);
    let text = if block.links.is_empty() {
        SelectableText::new(id.clone(), shown.clone(), styled)
    } else {
        let ranges = block.links.iter().map(|link| link.range.clone()).collect();
        let links = block.links.clone();
        let tooltip_links = block.links.clone();
        let element_id = id.clone();
        SelectableText::wrapping(id.clone(), shown.clone(), styled, move |styled| {
            InteractiveText::new(element_id, styled)
                .on_click(ranges, move |index, _, cx| {
                    if let Some(link) = links.get(index) {
                        link.target.open(cx);
                    }
                })
                .tooltip(move |index, window, cx| {
                    tooltip_links
                        .iter()
                        .find(|link| link.range.contains(&index))
                        .map(|link| tooltip(link.target.label(), window, cx))
                })
                .into_any_element()
        })
    }
    .tight(tight);
    let body = div()
        .debug_selector(|| "markdown.paragraph".into())
        .min_w_0()
        .flex_1()
        .whitespace_normal()
        .fx_role(role)
        .text_color(Colors::fg(&theme))
        .child(text);
    if let BlockKind::Code(language) = &block.kind {
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px_len(Spacing::SM))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .fx_caption()
                    .child(SharedString::from(language.clone())),
            )
            .child(copy_code_button(shown));
        let code = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .p(px_len(Spacing::MD))
            .child(header)
            .child(body);
        // An id of its own, so two fenced blocks in one message do not share
        // their copy buttons' interaction state.
        return div()
            .id(SharedString::from(format!("{id}:code")))
            .min_w_0()
            .w_full()
            .child(panel(code))
            .into_any_element();
    }
    let mut row = div()
        .min_w_0()
        .w_full()
        .flex()
        .flex_row()
        .gap(px_len(Spacing::SM));
    if block.list_depth > 0 {
        row = row.pl(px_len(
            Spacing::LG * block.list_depth.saturating_sub(1).min(8) as f32,
        ));
        row = row.child(
            div()
                .min_w(px_len(Spacing::LG))
                .flex_shrink_0()
                .fx_body()
                .child(SharedString::from(block.marker.clone().unwrap_or_default())),
        );
    }
    if block.quote_depth > 0 {
        row = row
            .border_l(px_len(BorderWidth::STRONG))
            .border_color(Colors::border_medium(&theme))
            .pl(px_len(Spacing::MD));
    }
    row.child(body).into_any_element()
}

/// The button that copies a block of code exactly as written.
///
/// Shared with the transcript's own code blocks, so every block of code offers
/// the same control in the same place.
pub(crate) fn copy_code_button(code: SharedString) -> AnyElement {
    icon_button(
        IconButtonProps::new(Icon::DOC_ON_DOC, "Copy code")
            .tiny()
            .ghost(),
        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(code.to_string())),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, MouseButton, Render, TestAppContext, VisualTestContext, Window, point, px, size,
    };

    #[test]
    fn deployment_link_uses_its_label_and_keeps_the_exact_destination() {
        let target = "https://github.com/xcrap/azoresgame/actions/runs/38328432034";
        let blocks = parse(&format!(
            "[Deployment succeeded]({target}). Updated `game.rs`."
        ));
        let block = &blocks[0];
        assert_eq!(block.text, "Deployment succeeded. Updated game.rs.");
        assert_eq!(
            block.links,
            vec![Link {
                range: 0..20,
                target: LinkTarget::Web(target.into())
            }]
        );
        assert_eq!(
            &block.text[block
                .spans
                .iter()
                .find(|span| span.style.code)
                .unwrap()
                .range
                .clone()],
            "game.rs"
        );
    }

    #[test]
    fn nested_unicode_spans_cover_displayed_text_without_overlap() {
        let blocks = parse("Olá **bold [*Açores* `é.rs`](https://example.com/test)** ~~old~~.");
        let block = &blocks[0];
        assert_eq!(block.text, "Olá bold Açores é.rs old.");
        let mut next = 0;
        for span in &block.spans {
            assert_eq!(span.range.start, next);
            assert!(block.text.is_char_boundary(span.range.start));
            assert!(block.text.is_char_boundary(span.range.end));
            next = span.range.end;
        }
        assert_eq!(next, block.text.len());
        assert_eq!(&block.text[block.links[0].range.clone()], "Açores é.rs");
        let code = block.spans.iter().find(|span| span.style.code).unwrap();
        assert!(code.style.strong && code.style.link);
        let italic = block.spans.iter().find(|span| span.style.emphasis).unwrap();
        assert!(italic.style.strong && italic.style.link);
    }

    #[test]
    fn paragraphs_lists_quotes_and_fences_keep_their_structure() {
        let blocks = parse(
            "## Changes\n\n- first\n  - nested\n- second\n\n3. third\n4. fourth\n\n> a quote\n\n```rust\nlet path = `[not a link](https://example.com)`;\n```\n\nDone.",
        );
        assert_eq!(blocks[0].kind, BlockKind::Heading(2));
        assert_eq!(blocks[1].marker.as_deref(), Some("•"));
        assert_eq!(blocks[2].list_depth, 2);
        assert_eq!(blocks[3].text, "second");
        assert_eq!(blocks[4].marker.as_deref(), Some("3."));
        assert_eq!(blocks[5].marker.as_deref(), Some("4."));
        assert_eq!(blocks[6].quote_depth, 1);
        assert_eq!(blocks[7].kind, BlockKind::Code("rust".into()));
        assert!(blocks[7].text.contains("`[not a link]"));
        assert!(blocks[7].links.is_empty());
        assert_eq!(blocks[8].text, "Done.");
    }

    #[test]
    fn rejected_link_schemes_stay_readable_and_never_become_clickable() {
        for target in [
            "javascript:alert%281%29",
            "data:text/html,test",
            "file:///tmp/test",
            "command:run",
            "vscode://file/tmp/test",
            "//example.com/test",
            "relative.rs",
        ] {
            let blocks = parse(&format!("[readable label]({target})"));
            assert_eq!(blocks[0].text, "readable label", "{target}");
            assert!(blocks[0].links.is_empty(), "{target}");
            assert!(
                blocks[0].spans.iter().all(|span| !span.style.link),
                "{target}"
            );
        }
        assert!(LinkTarget::parse("https://exa\nmple.com").is_none());
        assert!(LinkTarget::parse("https://").is_none());
    }

    #[test]
    fn absolute_file_links_decode_spaces_and_strip_line_references() {
        assert_eq!(
            LinkTarget::parse("/Users/me/My%20Project/main.rs:12"),
            Some(LinkTarget::Local(PathBuf::from(
                "/Users/me/My Project/main.rs"
            )))
        );
        assert_eq!(
            LinkTarget::parse("/tmp/data:backup"),
            Some(LinkTarget::Local(PathBuf::from("/tmp/data:backup")))
        );
        assert!(LinkTarget::parse("//server/shared/path").is_none());
        assert!(LinkTarget::parse("/tmp/invalid%00path").is_none());
    }

    #[test]
    fn incomplete_streaming_markdown_preserves_the_received_text() {
        let blocks = parse("Working on **a change with [a pending link](https://example");
        assert_eq!(
            blocks[0].text,
            "Working on **a change with [a pending link](https://example"
        );
        assert!(blocks[0].links.is_empty());
        assert_eq!(
            parse("A\nsoft break  \nand hard break")[0].text,
            "A soft break\nand hard break"
        );
    }

    #[test]
    fn a_transcript_with_more_blocks_than_the_old_cap_is_parsed_once() {
        // The old cache kept 64 entries in recency order, and every pass walks
        // every block, so block 65 evicted block 1 just before it was needed.
        let mut cache = ParseCache::default();
        let blocks: Vec<(SharedString, String)> = (0..300)
            .map(|index| {
                (
                    SharedString::from(format!("message:{index}")),
                    format!("Paragraph **{index}** of a long conversation."),
                )
            })
            .collect();
        for _ in 0..3 {
            cache.pass += 1;
            for (id, source) in &blocks {
                cache.get(id, source);
            }
        }
        assert_eq!(cache.parses, blocks.len(), "a repaint re-parsed blocks");
    }

    #[test]
    fn an_oversized_conversation_reparses_only_its_overflow() {
        // Four of these fit the budget; six do not. Plain LRU would evict each
        // entry just before the pass reached it again and parse all six.
        let mut cache = ParseCache::default();
        let block = "x".repeat(MAX_CACHED_SOURCE_BYTES / 4 - 1);
        let ids: Vec<SharedString> = (0..6)
            .map(|index| SharedString::from(format!("big:{index}")))
            .collect();
        for _ in 0..2 {
            cache.pass += 1;
            for id in &ids {
                cache.get(id, &block);
            }
        }
        assert_eq!(cache.parses, 6 + 2, "the cached part was evicted mid-pass");
        assert!(cache.bytes <= MAX_CACHED_SOURCE_BYTES);
    }

    #[test]
    fn a_thread_the_reader_left_makes_room_for_the_one_on_screen() {
        let mut cache = ParseCache::default();
        let block = "y".repeat(MAX_ENTRY_SOURCE_BYTES);
        let fits = MAX_CACHED_SOURCE_BYTES / MAX_ENTRY_SOURCE_BYTES;
        cache.pass += 1;
        for index in 0..fits {
            cache.get(&format!("left:{index}").into(), &block);
        }
        cache.pass += 1;
        for index in 0..fits {
            cache.get(&format!("shown:{index}").into(), &block);
        }
        for index in 0..fits {
            assert!(
                cache
                    .entries
                    .contains_key(format!("shown:{index}").as_str())
            );
            assert!(!cache.entries.contains_key(format!("left:{index}").as_str()));
        }
        assert!(cache.bytes <= MAX_CACHED_SOURCE_BYTES);
    }

    #[test]
    fn a_changed_source_replaces_its_entry_instead_of_accumulating() {
        // A streamed block keeps one id while its text grows on every delta.
        let mut cache = ParseCache::default();
        let id = SharedString::from("streaming");
        for length in 1..50 {
            cache.get(&id, &"z".repeat(length));
        }
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, 49);
    }

    #[test]
    fn a_fenced_code_block_copies_its_code_exactly() {
        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Harness {
            source: "Before.\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```".into(),
        });
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(400.0), px(600.0)));
        test.refresh().unwrap();
        let button = visual
            .debug_bounds("icon-button:Copy code")
            .expect("a code block offers a copy button")
            .center();
        visual.simulate_mouse_down(button, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(button, MouseButton::Left, Default::default());
        assert_eq!(
            test.read_from_clipboard().and_then(|item| item.text()),
            Some("fn main() {\n    println!(\"hi\");\n}".to_string())
        );
    }

    struct Harness {
        source: String,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w_full().child(render(&self.source, "markdown-test"))
        }
    }

    #[test]
    fn a_wrapped_link_is_clickable_on_its_last_line() {
        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Harness {
            source: "[Deployment succeeded and every changed page is ready to review in the browser](https://example.com/deployment)".into(),
        });
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(190.0), px(600.0)));
        test.refresh().unwrap();
        let bounds = visual.debug_bounds("markdown.paragraph").unwrap();
        assert!(bounds.size.width <= px(190.0));
        assert!(
            bounds.size.height > px(50.0),
            "link should wrap: {bounds:?}"
        );
        let position = point(bounds.origin.x + px(5.0), bounds.bottom() - px(7.0));
        visual.simulate_mouse_move(position, None, Default::default());
        visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
        test.refresh().unwrap();
        visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
        assert_eq!(
            test.opened_url().as_deref(),
            Some("https://example.com/deployment")
        );
    }

    #[test]
    fn a_fenced_code_block_keeps_its_lines_and_fits_a_narrow_column() {
        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Harness {
            source: "```rust\nlet first = 1;\n    let second = 2;\n```".into(),
        });
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(190.0), px(600.0)));
        test.refresh().unwrap();
        let bounds = visual.debug_bounds("markdown.paragraph").unwrap();
        assert!(bounds.size.width <= px(190.0));
        assert!(
            bounds.size.height >= px(30.0),
            "code lines lost their height: {bounds:?}"
        );
    }
}
