# FlowX (Rust)

A native macOS AI workspace for orchestrating conversations, browser previews,
git inspection, and terminals — a port of the Swift original at
`../_flowx` to Rust and [GPUI](https://gpui.rs).

## Toolchain

- Rust 1.98.1, pinned in `rust-toolchain.toml` to match the version Zed builds
  against.
- `gpui = "0.2.2"` from crates.io. Deliberately the published release rather
  than a git dependency: the API does not churn, which matters when tens of
  thousands of lines sit on top of it.
- macOS with Metal. GPUI needs `font-kit` for glyph rasterization; that is
  already a default feature.
- Zig 0.17.0, only for phase 2 (`libghostty-vt` compiles the Ghostty VT core
  from Zig source at build time).

## Build

```sh
./fx build      # compile the whole workspace, tests included
./fx run        # run the app in place
./fx launch     # assemble a .app bundle and open it
./fx test       # run every test
./fx bench      # run the benchmarks
./fx lint       # fmt check + clippy with warnings denied
./fx clean      # remove target/
```

`./fx launch` produces `target/debug/bundle/FlowX-Dev.app`. A real bundle
matters: the app needs one to own a Dock icon, a menu bar, and an
application-support directory.

## Crate layout

Dependencies point one way only. Nothing lower in this list may depend on
anything above it.

| Crate | Ported from | Depends on | Responsibility |
|---|---|---|---|
| `fx-core` | `Packages/FXCore` | — | Domain models, pure policy services, git parsing, runtime discovery, image preparation. No UI, no GPUI. |
| `fx-design` | `Packages/FXDesign` | gpui | The design system: tokens, typography, icons, shared controls. The source of truth for how FlowX looks. |
| `fx-agent` | `Packages/FXAgent` | fx-core | Providers (Codex, Claude), the conversation engine, streaming, approvals. |
| `fx-store` | `FlowX/Services` | fx-core | Persistence. Owns the on-disk format and nothing else. |
| `fx-term` | `Packages/FXTerminal` | fx-design | Terminal sessions and rendering. Phase 2. |
| `fx-app` | `FlowX/State`, `FlowX/Views` | all of the above | Application state, the window shell, and every view. |
| `flowx` | — | fx-app | A `main` that opens the window. Nothing else. |

Two rules keep this structure intact:

- **`fx-core` never imports gpui.** Anything that needs to draw belongs in
  `fx-design` or `fx-app`. This is what keeps the pure logic testable without a
  window.
- **`fx-design` never imports `fx-core`.** The design system must stay usable by
  any crate, including the terminal.

## Design system

`fx-design` is the code-level source of truth for FlowX interface styling and
components. Implement UI by extending it first, then consume from app views.

### Color architecture

The system works like CSS custom properties — semantic tokens that resolve from
the active theme at draw time:

```
Colors::fg(&theme)          what views read
       ↓
Theme { base_tone, appearance, accent, text_size }
       ↓
Palette::get(tone, dark)    the backing layer
       ↓
ToneScale                   11 shades per tone, 50 → 950
```

- **Views never hardcode a color.** If a token is missing, add it to
  `theme::color::Colors` first.
- Base tones are slate, zinc, neutral, and stone. Dark reads top-down (900 =
  background, 800 = elevated, 700 = surface); light reads bottom-up (50, 100,
  200). All eight combinations are generated once into a static table.
- Accents (violet, blue, emerald, orange, rose) are independent of the tone.
  `on_accent` is white, or the tone's darkest ink for emerald and orange, which
  cannot carry white text.
- Semantic, muted, and diff colors adapt per mode.

### Invariants that must keep holding

`crates/fx-design/tests/design_tokens.rs` enforces these. Rerun it after
touching any foreground, background, or accent token.

- `fg` clears 7:1 on `bg` and `bg_elevated`; `fg_secondary` and `fg_tertiary`
  clear 4.5:1 there, and stay in that order.
- `fg_tertiary` clears 3:1 on `bg_surface`.
- `on_accent` clears 3:1 on every accent, in every tone and mode.
- Banner text (`fg`, `fg_secondary`) clears 4.5:1 on every `*_muted` fill.
- Diff foregrounds clear 3:1 on their own diff backgrounds.

One documented shortfall, recorded rather than hidden: where a status hue is
used as an *icon on its own tint*, light-mode `warning` bottoms out at 2.29:1
and `success` at 2.67:1, against the 3:1 non-text guideline. Dark mode clears
3:1 everywhere. The floor is pinned by
`status_ink_meets_its_documented_floor_on_its_own_tint` so it cannot get worse.
Raising it means darkening the light-mode values — a palette change, not a port.

Run the measurement yourself after any retune:

```sh
cargo run -p fx-design --example contrast_report
```

### Typography

SF Pro Rounded for UI text, SF Mono for code, terminal output, and diff line
numbers. Sizes come from `TextRole` and are multiplied by `TextSizePreset`; no
view writes a point size. Every role also carries a line-height multiplier
because GPUI has no automatic line height.

### Spacing, radii, borders, elevation

8px baseline grid (`Spacing`), `Radii`, `BorderWidth` (hairline 0.5 by default),
and `Shadow` recipes. Apply elevation with `.fx_shadow(...)`; never write a raw
`shadow(...)`.

### Components

`fx-design/src/components/` holds the shared control vocabulary. Extend it
before styling a screen ad hoc.

| Module | Provides |
|---|---|
| `button` | Labelled button (primary/secondary/ghost/danger), icon button, compact action button. Hover and press are declarative `.hover`/`.active` refinements, so a button repaints without re-rendering. |
| `surface` | Card, panel, divider, banner, empty state, section header. |
| `status` | Badge, activity dot, typing indicator, pulsing dot, context-usage ring. |
| `segmented` | Segmented control for exclusive choices. |
| `tooltip` | Hover tooltip body. |
| `menu` | Dropdown and context-menu panels, plus `MenuLayer`, the one-open-menu-at-a-time owner. |
| `menu_model` | `MenuItem` / `MenuSection` / `MenuModel`, and the highlight and keyboard-navigation rules. |

Color decisions live in the `*_palette` and `*_ink` helpers rather than inline
in the element tree, so they can be asserted without a window. Prefer those
functions; the element builders just apply them.

### Menus

The Swift original rejected `NSMenu` and built flat panels on an `NSPanel`,
which meant every open panel also owned a separate window, a pair of event
monitors, and an `NSHashTable` hit-test tree for nested context menus.

GPUI makes all of that unnecessary: `anchored()` positions a panel against its
anchor and keeps it inside the window, so a popup is one absolutely positioned
element in the same window as its content. What that leaves behind is the
*decision* half, which is split in two:

- **`MenuModel`** is pure state — sections, items, and the highlight key.
  Keyboard movement, wrapping, disabled-item skipping, and activation are all
  methods on it, so they are tested without a window.
- **`MenuLayer`** is the app-wide owner of *which* menu is open. Only one at a
  time, so opening a second closes the first. It stores no callbacks: opening
  and closing return what happened, and the caller reacts.

Dismissal needs no global monitors. A transparent backdrop covers the window and
closes on mouse-down; the panel is stacked above it.

Two things are deliberately preserved from the original:

- The panel is sized to its content and gets an edge rather than a shadow, since
  a styled element shadow is clipped to the element's own bounds.
- Pointer and keyboard share one highlight, so pressing a row and then pressing
  an arrow key continues from the pointer rather than from the top of the menu.

Two behaviours are deliberate departures from the Swift original, both because
GPUI cannot express the original:

- A pressed button dims to 0.94 opacity instead of scaling to 0.97. GPUI's
  style refinement has no transform, and animating a scale would re-run layout.
- The activity spinner is drawn as twelve rotated capsules rather than a
  trimmed stroke, for the same reason.

One accessibility improvement: `BadgeTone::text_ink` uses the tone's hue for
the label only where it clears AA against its own fill, and falls back to
`fg` where it does not. Light-mode `warning` over its own tint is 2.6:1, which
is unreadable at 12pt. The tint still carries the tone, so the badge keeps its
identity without shipping an illegible label.

### Icons

`fx-design/src/icon.rs` exposes glyphs under the **same SF Symbols names the
Swift app used**, so call sites port without renaming — `Icon::CHEVRON_DOWN`,
`Icon::EXCLAMATIONMARK_TRIANGLE_FILL`, and so on.

Glyphs are Lucide (ISC), vendored in `crates/fx-design/assets/icons/` and embedded
by the build script. **GPUI's `Svg::path` takes an asset key, not SVG drawing
instructions.** `Icon::render` supplies `icons/<name>.svg`, and the application
installs `IconAssets` with `Application::with_assets`. GPUI parses and caches the
original SVG, preserving its stroke geometry without runtime filesystem reads.
Flattened path data remains available for geometry checks and `Icon::path`
compatibility; it must never be passed directly to `Svg::path`.

To add an icon: drop `<sf-symbol-name>.svg` into `assets/icons/`, add a constant
to the `icons!` macro at the bottom of `icon.rs`. The tests check that the table
stays sorted (lookup binary-searches), that names stay unique, and that no
coordinate escapes the 24×24 viewBox. They also resolve every embedded asset and
rasterize it through GPUI's SVG engine to verify that it paints visible pixels.

Lucide is stroke-only, so a few Swift `.fill` variants render as outlines. That
is consistent with FlowX's flat treatment, not a regression.

## GPUI 0.2.2 API notes

The published crate's API differs from the current gpui.rs documentation. Where
they disagree, this crate follows 0.2.2. The differences that actually bite:

- Colors are `Rgba` (and `Hsla`). There is no `Color` type.
- `WindowOptions` takes `window_bounds: Option<WindowBounds>`, `titlebar:
  Option<TitlebarOptions>`, and `window_background:
  WindowBackgroundAppearance`. There are no `bounds` or `title` fields.
- `Bounds::new` takes `Point` and `Size`, not four numbers. Construct with
  `gpui::point`, `gpui::size`, and `gpui::px`.
- `Pixels` has a private field; always go through `gpui::px`.
- `Application::run` takes a single `&mut App` closure.
- `Render::render` takes `&mut Context<Self>`, not `&mut App`.
- Shadows are `shadow(Vec<BoxShadow>)`, not `box_shadow(offset, blur, color)`.

### Interactive elements need an id

This is the one that costs the most time to rediscover. GPUI splits
interactivity across two traits:

| Available on a plain `div()` | Requires `div().id(..)` first |
|---|---|
| `hover`, `on_mouse_down`, `on_mouse_move`, `on_key_down`, `group_hover`, `occlude`, `on_drop` | `on_click`, `active`, `tooltip`, `hoverable_tooltip`, `overflow_scroll`, `track_scroll`, `focusable` |

`.id(..)` returns `Stateful<Div>`, which is what implements
`StatefulInteractiveElement`. So a button reads:

```rust
div().id("send").hover(..).active(..).on_click(..)
```

Both traits come from `gpui::prelude::*`, which every component file imports.

### Style refinements

`.hover(..)` and `.active(..)` take a callback returning a `StyleRefinement`,
where every field is an `Option` and the nested `text` style is itself an
`Option`. Use the `FxRefinement` helpers from `fx-design::styled` rather than
touching that shape directly:

```rust
.hover(|s| s.fx_background(hover_bg).fx_text_color(hover_fg))
```

### Other gotchas worth knowing

- There is no `scale`, `rotate`, or transform on a styled element. An element
  can only be laid out and painted. Animated emphasis has to come from opacity,
  size, or a custom paint pass.
- A tooltip body must be a real view (`Entity<V>` converted with `AnyView::from`),
  not an element tree.
- `ElementId` converts from `SharedString` and `&'static str`, not from `String`.

## Persistence

`fx-store` owns the on-disk format and nothing else. Three rules follow from that,
and each one exists because its alternative was tried first.

- **The format depends on `fx-core`, never on `fx-agent`.** A persisted record is
  built from `Vec<ConversationMessage>`, not from an engine conversation, so the
  format outlives any particular engine.
- **Every write is atomic and leaves a `.backup` sibling.** The primary lands
  before the copy is replaced, so a crash between the two leaves the previous
  good copy rather than losing both.
- **A read falls back on a parse failure, not only a read failure.** An
  interrupted write leaves a truncated file that opens fine and then does not
  parse; a fallback that only triggers on `read` would never fire for the damage
  it was added to survive.

Files under the application-support directory are created `0700`. Everything
there can contain prompts and code, so it does not inherit a wider mode.

An empty transcript is **deleted** on quit rather than written as a stub: an
unopened provider thread has nothing to save, and writing it anyway would
truncate every other thread's cache.

One cost of the crate boundary: `fx-store` cannot import `fx-design`, so the
inspector's geometry is declared in both `PersistedApp` and `Layout`.
`the_persisted_defaults_match_the_design_metrics` in `fx-app` is the only test
that sees both, and is therefore the only place the duplication can be caught.

## Runtime discovery

Neither provider CLI is bundled. Both are the user's own install, found at launch
by `fx-core::services::runtime`, which makes it less about spawning a process and
more about deciding whether a file on disk is safe to execute.

Three rules, each of which is a bug that was written first:

- **A GUI app cannot trust its own `PATH`.** Launched from Finder, a process
  inherits launchd's minimal `PATH`. The search therefore has its own list, and
  every launch rebuilds `PATH` around the directory the binary was found in — an
  npm-installed provider's script needs the interpreter beside it. Empty `PATH`
  entries are dropped, because an empty component means the working directory.
- **Quarantine is never bypassed.** A downloaded binary carries
  `com.apple.quarantine` until a human clears it. FlowX reports it and stops. The
  one exception is an injectable assessor, which exists for a signature check on
  a notarized build. The attribute reader is injectable for the same reason: the
  policy cannot be tested if setting up the condition needs `xattr`.
- **Indirection is resolved before the quarantine check**, so a clean-looking
  symlink cannot hide a quarantined target. The reported path is the resolved one,
  so swapping a link later cannot redirect a launch.

A quarantined binary is skipped, not fatal: if a working one exists later in the
search it is used. That is a real case — on the machine this was written on, the
Homebrew cask `codex` is quarantined while a `~/.local/bin` install is not.

What counts as quarantined is narrower than "has the attribute", for two reasons:

- **A notarized binary passes.** The default assessor is Swift's
  `SecStaticCodeCheckValidity` requirement (Apple, or Developer ID + notarized),
  remembered per file identity. That same Homebrew `codex` is quarantined and
  notarized, and is accepted.
- **An approval keeps the attribute.** Approving a binary in Finder sets the
  user-approved flag (`0x40`) in the value rather than removing it, so the value
  is parsed. Checking presence alone meant the approval FlowX asks for could
  never clear it.

The extra directories are directories: the candidate is `<dir>/<name>`. Checking
the directory itself always missed, and the search silently fell through to a
login shell that does not read `~/.zshrc`.

Discovery tests run with `without_extra_directories`. Without it a "not found"
assertion can be satisfied, or defeated, by whatever happens to be installed on
the machine running the suite.

Two of these were found by running the stack against the real `codex
app-server` rather than by testing, and both are now pinned by tests:

- **A directory is not a binary.** Checking only the execute bit selects
  `/opt/homebrew/bin` as the `codex` binary — every preferred search directory
  carries that bit — and the launch then fails with a permission error on a
  directory.
- **Codex sends thread timestamps in seconds, not milliseconds.** Read as
  milliseconds, a same-day thread lands in the year 58656. The live payload also
  calls the title `name`, not `title`.

An invented test fixture gets both of these wrong and asserts the wrong thing
confidently. The captured real payload is in `a_live_thread_shape_is_read_correctly`.

## Provider transport

`codex app-server` and `claude` both speak newline-delimited JSON-RPC on stdio.
The Swift original ran this as one 4,000-line actor holding the process, the
pending-response map, the interrupt watchdog, and every protocol method. It is
split here so the failure paths are reachable from a test:

- `LineFramer` — pure, no IO.
- `PendingRequests` — who is waiting on what, and how a failure is broadcast.
- `Transport` — the process and its two reader tasks.

The process tests spawn a real `python3` fake server. A mocked stream cannot reach
the things most likely to break: handshake order, response correlation over an
actual pipe, and what a caller sees when the process dies mid-request.

Four rules cost real time to get right, and each has a test:

- **`initialize` carries the reserved id 0**; dynamic ids start at 10. The server
  treats low ids as handshake traffic.
- **`initialized` is sent only after `initialize` succeeds.** Sending it earlier
  is a race the server loses.
- **An exit and a rejection are different errors.** `Outcome::Answered` and
  `Outcome::Dead` exist because funnelling both through one error type made a
  dead server report itself as a rejected method.
- **A request against a dead or closed process fails immediately.** Otherwise
  every request after a crash waits out the full 30-second control timeout with
  the composer disabled.

`LineFramer` remembers how far it has scanned. Without that, a line arriving in
many chunks is rescanned from the front each time — quadratic, which for a 64 MB
message is billions of comparisons. The `scan_offset` field in the Swift original
existed for the same reason, and its test asserts a time bound rather than only
correctness.

## The transcript

The transcript carries tool calls as first-class rows, not only as the activity
strip above it. Four rules, each of which is a bug that was written first:

- **`Done` commits the streaming buffer, never clears it.** Clearing discards the
  turn's last — often only — sentence, so the final answer never appears.
- **A tool call splits the prose.** The streamed text is committed, the call is
  appended, and a new segment starts. That is the whole interleaving mechanism:
  "let me check" / call / result / "found it" is four rows, not one row with the
  call buried in it.
- **A successful result saying nothing is not recorded.** A tool that returned
  empty, or the literal `Completed`, would otherwise add a row per call and bury
  the prose. Errors are always recorded.
- **`claim_tool_id` must not gate the append.** The `ToolUse` and the matching
  `ToolResult` carry the *same* id, so a dedup gate on it rejects every legitimate
  first result. It tracks completion for the spinner; that is all.

Tool output is read from `aggregatedOutput` first, then `output`, `result`,
`error`, `changes` — that order is the server's. Reading only `output`/`result`
yields an empty result for every real tool call, which then looks trivial and is
dropped, so the transcript silently loses every tool output.

Whether a row is prose or a collapsed activity is a *view* decision, made by
`TranscriptActivityGroupingPolicy` in `fx-core`. It reads content only, never
role; the caller excludes user messages before asking it to segment.

## Two queues, and why they are bounded

Both transports cap their channels rather than letting them grow. The reason is
the same on each side: a session outlives its turn, and nothing drains the queue
between turns, so an unbounded one grows for as long as a thread sits idle.

When the queue fills, the reader stops reading and the server's pipe fills, so it
waits. That is deliberate: a stall is a truthful signal that FlowX is not
keeping up, where unbounded growth is silent and shows up later as an
out-of-memory.

One trap worth writing down, because it costs an hour: **`mpsc::Sender::send` on
a *bounded* channel is a future.** Converting `unbounded_channel` to `channel`
leaves every `let _ = sender.send(..)` compiling — and silently delivering
nothing. Both `send` sites were found that way, by tests that had passed all
along.

Claude's stream reader is exclusive by construction: `reader()` moves the
receiver out, so there is exactly one consumer. Answering an approval stays on
the transport, because that genuinely happens from another task while the stream
is being read.

## The shell

`fx-app` is three columns — sidebar, centre, inspector — and it has exactly two
app-wide owners, both held by `AppState`:

- **`MenuLayer`** holds at most one open menu. Drawing it is a separate concern
  from owning it: `menu_layer_overlay` is called once at the root, which is why a
  context menu opened from a row appears above the same content as a dropdown
  opened from a toolbar button, and why neither can be clipped by the column it
  came from.
- **`ContextRegistry`** holds the regions that offer a context menu. The
  right-click handler is attached to the **root**, not per region, so
  right-clicking the background does nothing instead of opening an empty panel.

The deepest registered region is authoritative. A region with no menu blocks its
ancestors rather than falling through, because falling through would offer an
ancestor's actions for a target the ancestor does not own.

`AppHandle` is a `gpui::Global` wrapping the state entity. A menu row's callback
receives only a `Window` and an `App`, so the state has to be reachable without
being captured.

### Text input

GPUI 0.2.2 ships no text-input element. `crates/fx-app/src/text_field.rs` is
an `Entity<TextField>` implementing `gpui::platform::InputHandler`, registered
during paint through `Window::handle_input`. Of that trait's methods only
`apple_press_and_hold_enabled` has a default; the other seven are required.

Two decisions there are load-bearing:

- **Printable characters are not claimed in `on_key_down`.** They arrive through
  `NSInputContext` → `replace_text_in_range`, which is what makes dead keys and
  IME composition work. Claiming them would insert the character twice.
- **`Return` inserts a newline in a multi-line field and only submits in a
  single-line one.** Cmd/Ctrl-Return submits in both, and is claimed explicitly
  because macOS reports `key_char: Some("\n")` for it.

### Running a turn

The runner owns a `CodexSession` for a turn's lifetime and writes events into
the conversation from a background thread, so the conversation is behind a
`Mutex` — the render path holds it only for the read, never across an await.

The runner's callback **may not capture `App`**: it fires on another thread and
`App` is neither `Send` nor reachable from there. It sends to an `mpsc` channel
instead, which `AppState::drain_updates` reads at the top of every render. That
is also what makes a finished turn repaint without anything asking it to.

`DenyUnlessReadOnly` only bites when the thread is opened at a level that asks:
`AgentAccess::FullAccess` maps to `("never", "danger-full-access")`, so Codex
never asks and FlowX never gets to refuse. The access level has to be
`Supervised` or `AcceptEdits` for the policy to mean anything.

### Scrolling

GPUI 0.2.2's scrollable elements have **no keyboard handling at all**. The
Swift original got arrows and page keys from `NSScrollView` for free, so the
transcript adds `.track_focus()` plus an `on_key_down` that pages the handle —
claiming only the keys it moves, and stopping propagation for those alone.

`track_scroll` reaches `Window::current_view()`, which unwraps a stack that only
`View::paint` populates. That means **a transcript cannot be painted through
`VisualTestContext::draw`** — a bare element is not a view. Mount a one-line
`Render` probe and refresh instead; that is what `paint()` in the `fx-app` tests
does.

`Window::request_animation_frame()` is unsafe from a render function for the
same reason — it calls `current_view()` and asserts the draw phase. Use
`window.on_next_frame(…)`, and key it on the caret's blink phase rather than on
every pass, so a fast stream cannot turn it into a frame per delta.

### View tests

GPUI's test platform is behind the `test-support` feature, which is enabled from
`fx-app`'s **dev-dependencies** rather than from the workspace. Adding it to the
workspace dependency would pull leak detection and a fake platform into the
shipped binary.

Each view test builds its own `TestAppContext` and its own `TempDir`. Sharing one
store directory across tests is a race: they run concurrently in one process and
overwrite each other's snapshot, which surfaces as a test that fails only in a
full run.

## Porting conventions

- Swift `FXColors.foo` becomes `Colors::foo(&theme)`.
- Swift `FXSpacing.sm` becomes `Spacing::SM`. Same for `Radii`, `BorderWidth`,
  `FXLayout` → `Layout`, `FXAnimation` → `FxAnimation`.
- Swift `FXTypography.body` becomes `Typography::style(TextRole::Body, &theme)`,
  applied with `.fx_body()`.
- Swift `FXShadow.popover` becomes `Shadow::popover(Colors::overlay(&theme))`.
- Pure logic ports almost verbatim, keeping its tests. These are the cheapest and
  highest-value parts of the port.
- `enum` cases become `PascalCase`; acronyms keep theirs (`CHEVRON_DOWN`,
  `is_dark`).
- Errors are `thiserror` enums in the crate that owns the failure, surfaced
  with `anyhow` only at the binary edge.

## Deferred to phase 2

- **Terminal.** SwiftTerm is a Ghostty fork. Phase 2 uses `libghostty-vt`, safe
  bindings over the same VT core, plus a GPUI renderer. `Xuanwo/gpui-ghostty`
  is a working reference. Requires Zig at build time.
- **Browser split.** Swift owned a real `WKWebView`; nothing in Rust embeds
  WebKit. Deferred pending a decision between an `objc2` bridge and dropping the
  embedded preview for an external browser.
