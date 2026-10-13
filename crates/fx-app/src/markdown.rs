//! Markdown prose rendered by GPUI, without HTML or a browser surface.
//!
//! Parsing keeps byte ranges against the displayed text, not against Markdown
//! source. GPUI can then wrap and hit-test an entire paragraph, including links
//! spanning several lines, without a separate layout box for every span.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use fx_design::components::rich_text::{InlineSpan, InlineStyle, rich_text};
use fx_design::components::{divider, panel, tooltip};
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{AnyElement, App, InteractiveText, SharedString, div};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use url::Url;

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

struct CachedMarkdown {
    id: SharedString,
    source: String,
    blocks: Rc<Vec<Block>>,
}

#[derive(Default)]
struct ParseCache {
    entries: VecDeque<CachedMarkdown>,
    bytes: usize,
}

thread_local! {
    // Transcript refreshes also happen for caret and activity animation. Keep
    // unchanged paragraphs parsed, with bounds across all visited chats.
    static PARSE_CACHE: RefCell<ParseCache> = RefCell::new(ParseCache::default());
}

fn cached_parse(source: &str, id: &SharedString) -> Rc<Vec<Block>> {
    const MAX_ENTRIES: usize = 64;
    const MAX_SOURCE_BYTES: usize = 1_048_576;
    PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.entries.iter().position(|entry| &entry.id == id) {
            let entry = cache.entries.remove(index).expect("found cache entry");
            if entry.source == source {
                let blocks = entry.blocks.clone();
                cache.entries.push_front(entry);
                return blocks;
            }
            cache.bytes -= entry.source.len();
        }
        let blocks = Rc::new(parse(source));
        if source.len() <= MAX_SOURCE_BYTES {
            cache.bytes += source.len();
            cache.entries.push_front(CachedMarkdown {
                id: id.clone(),
                source: source.to_owned(),
                blocks: blocks.clone(),
            });
            while cache.entries.len() > MAX_ENTRIES || cache.bytes > MAX_SOURCE_BYTES {
                if let Some(entry) = cache.entries.pop_back() {
                    cache.bytes -= entry.source.len();
                }
            }
        }
        blocks
    })
}

/// Render one Markdown content block. `id` must distinguish it from other
/// content blocks in the same transcript and stay stable while streaming.
pub(crate) fn render(source: &str, id: impl Into<SharedString>) -> AnyElement {
    let id = id.into();
    div()
        .id(id.clone())
        .min_w_0()
        .w_full()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .children(
            cached_parse(source, &id)
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, block)| {
                    render_block(block, SharedString::from(format!("{id}:{index}")))
                }),
        )
        .into_any_element()
}

fn render_block(block: Block, id: SharedString) -> AnyElement {
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
    let text = rich_text(SharedString::from(block.text), &block.spans, role, &theme);
    let text = if block.links.is_empty() {
        text.into_any_element()
    } else {
        let ranges = block.links.iter().map(|link| link.range.clone()).collect();
        let tooltip_links = block.links.clone();
        InteractiveText::new(id.clone(), text)
            .on_click(ranges, move |index, _, cx| {
                if let Some(link) = block.links.get(index) {
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
    };
    let body = div()
        .debug_selector(|| "markdown.paragraph".into())
        .min_w_0()
        .flex_1()
        .whitespace_normal()
        .fx_role(role)
        .text_color(Colors::fg(&theme))
        .child(text);
    if let BlockKind::Code(language) = block.kind {
        let code = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .p(px_len(Spacing::MD));
        let code = if language.is_empty() {
            code
        } else {
            code.child(div().fx_caption().child(SharedString::from(language)))
        };
        return div()
            .min_w_0()
            .w_full()
            .child(panel(code.child(body)))
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
                .child(SharedString::from(block.marker.unwrap_or_default())),
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
