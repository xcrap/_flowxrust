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
- Status glyphs clear 3:1 on their provider tile, the error glyph on the
  provider tint included, and the context meter's arc clears 3:1 on the
  composer at every fill level. Light-mode `warning` is too pale for that arc,
  so it uses `warning_ink`.
- Selected transcript text stays readable over `text_selection`.
- Checklist text clears 4.5:1 and every checklist mark 3:1 on its surface, with
  every accent. Light-mode emerald and orange fall short, so the active mark
  falls back to `fg` there, as a badge's label does.
- A badge's label clears 4.5:1 on its own fill over `bg`, `bg_elevated` *and*
  `bg_surface`, with every accent. The fill is translucent and badges sit on
  cards as well as panels; measured over the panel alone, dark "Ready" on a
  provider card was 4.2:1. In dark mode only neutral and warning keep their
  hue on both, so the others' labels are `fg` on their tint.
- The title bar's change counts clear 4.5:1 on the title bar at rest, hovered
  and selected. They use the diff inks, not `success`/`error` (light `success`
  was 3.4:1); dark `diff_removed_fg` is 4.2:1 on the selected fill, so it falls
  back to `fg` while the inspector is open.
- A menu's checkmark clears 3:1 on `bg_surface`, hovered or not, with every
  accent. Where the accent falls short (dark violet is 2.15:1), it is `fg`.

All four "hue where legible, `fg` where not" rules go through
`theme::legible_or`, given every fill the element can be drawn on — its color
does not change when the pointer or the surface under it does.

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
| `button` | Labelled button (primary/secondary/ghost/danger), icon button, compact action button (with an on/disabled variant, `compact_action_toggle`), and `glyph_button` for rows too dense for an icon button's 28 pt hit target. Hover and press are declarative `.hover`/`.active` refinements, so a button repaints without re-rendering. |
| `checkbox` | A labelled checkbox whose whole row toggles. |
| `checklist` | A read-only checklist: done, active (spinning while live) and pending rows on one surface. |
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
the label only where it clears AA against its own fill, over both a panel and a
card, and falls back to `fg` where it does not. Light-mode `warning` over its
own tint is 2.6:1, which is unreadable at 12pt. The tint still carries the
tone, so the badge keeps its identity without shipping an illegible label.

A selected row keeps its selection fill under the pointer — a thread row, a
settings destination, the title bar's change counts — as the segmented control,
`choice_row` and `compact_action_toggle` always did. GPUI's hover refinement
replaces the background rather than layering on it, so an unconditional
`.hover(bg_hover)` turned the 8% selection into a 4% wash.

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
- **One unreadable entry costs only that entry.** Lists decode element by
  element and unknown enum values read as the default — every one, the
  inspector tab and a goal's status included. Access levels are the exception:
  an unknown one reads as `Supervised`, because the default and the inherited
  app default are both Full Access. Lists that refer to entries (archived
  threads, pending titles) keep the ids of entries carried as raw JSON. An entry this build
  cannot read is kept as raw JSON and written back unchanged, so a newer build's
  data survives a round trip through an older one.

Both copies are written to a unique, owner-only temp file, flushed with
`F_FULLFSYNC` before the rename, and the directory is flushed after both
renames. A fixed temp name let two instances truncate each other's file.

Files under the application-support directory are created `0700`. Everything
there can contain prompts and code, so it does not inherit a wider mode.

An empty transcript is **deleted** on quit rather than written as a stub: an
unopened provider thread has nothing to save, and writing it anyway would
truncate every other thread's cache.

A queued prompt is saved with its transcript, images included, because
queuing clears the composer: a queue held only in memory was lost to every
quit. It comes back in the composer rather than as a queue. After a relaunch
nothing is running for it to wait behind, and FlowX never sends a prompt on
its own.

A binding records where its session starts in the transcript (`local_start`)
when FlowX opened that session in a thread that already had turns, as Restart
does. Native sync compares and replaces only from there. Compared whole, a
restarted thread's history could never match again, and the sidebar said
"Thread sync needs attention" on every pass. A prompt that replaces one the
provider never accepted takes that prompt's row for the same reason.

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
  `com.apple.quarantine` until a human clears it. FlowX reports it and stops,
  and never removes the attribute. The assessor and the attribute reader are
  injectable, because the policy cannot be tested if setting up the condition
  needs `xattr` or a signed binary.
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

The search paths are the Swift app's, in its order: Codex prefers the copy
bundled in ChatGPT.app or Codex.app, then Homebrew, `~/.local/bin`, `~/.npm/bin`
and nvm. A glob may sit mid-path (`~/.nvm/versions/node/*/bin/codex`), and
versions sort by number, so v22 wins over v9. The launch `PATH` leads with the
directory the binary was *found* in, then the resolved one: an nvm link resolves
into `lib/node_modules`, which has no `node` beside it.

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
  the composer disabled. That includes `initialize`: the exit watcher starts
  before the handshake, so a server that dies answering it fails at once, with
  its stderr.

Two more, for how a turn ends:

- **Silence is never a failure.** A turn ends on completion, a provider or
  process error, the stream closing, or Stop. A quiet `docker build` or a long
  think is not a hang, and a silence timeout killed exactly those turns.
- **An interrupted turn was stopped, not completed.** Another client, or Codex
  itself, can end a turn with `turn/completed` status `interrupted`; read as a
  completion it showed the Ready check and the unseen-completion dot for a turn
  that never answered. FlowX's own Stop never sees that status, because it
  drops the turn first.
- **Stop ends the tools too.** Each provider runs in its own process group
  (`fx-agent::process`), but its tools do not stay in it: `claude` starts each
  `Bash` call in a session of its own and `codex app-server` gives each command
  a group of its own. So Stop works on the provider's *tree* — its
  descendants, and every group and session one of them leads, which still
  finds a tool's children once launchd has adopted them. Stop sends the
  provider's interrupt, then SIGTERM to the tree, a short grace, then SIGKILL.
  A turn that finished normally only stops the server, so a dev server the
  agent started on purpose keeps running. The fakes start their tools the same
  way; a fake whose child stayed in the provider's group passed against a Stop
  that reached no real tool.
- **The tree is watched, not taken at Stop.** Taken at Stop, it missed every
  tool of a Claude that had already died, or been killed for an oversized
  line: with Claude gone, nothing in the table leads to a session it started.
  `process::Watched` records Claude's tree from spawn and re-reads it every
  500 ms and at each `task_started`, and Stop, a failed turn and Quit all end
  what it last saw joined with what the table shows now. A group or session is
  forgotten once empty, because a record that lives for hours would otherwise
  claim the next unrelated process to be handed its id.
- **Quit cannot await any of that.** GPUI waits a tenth of a second for quit
  handlers, so every transport registers its provider while it runs, and the
  quit handler calls `fx_agent::stop_all_now`, which ends every registered tree
  on the main thread.

Claude's background tasks (`Agent` or `Bash` with `run_in_background`) change
what "completion" means, and each rule here was a lost result first:

- **A `result` with a task running is not the end.** Claude reports the task's
  end as `task_notification`, then answers it as a turn of its own (`init`, text,
  a `result` whose `origin.kind` is `task-notification`). stdin stays open while
  a task runs *or an ended task's answer is still owed*, so the wait can be
  steered and the follow-up's permissions answered; it closes at the first
  `result` with nothing outstanding, before that line's events are delivered,
  and the turn then reads to the process's exit. A zero-turn acknowledgement
  after the prompt was answered counts as that answer. `BackgroundWait` in
  `session.rs` holds the turn's half of these rules.
- **Only an ending Claude tells the model is owed an answer.** A task that
  completed or failed is. One that was stopped (TaskStop, KillShell, a
  Monitor's timeout) is not: Claude marks a shell or monitor told as it kills
  it, and an agent the turn stopped is told inside that turn. Nor is one
  whose `task_started` says `owned_by_subagent`: its subagent is told. And one
  that ends while a turn still works is answered in that turn when a request
  follows it (`system` `status: requesting`, which Claude sends after gathering
  what is queued), and as a turn of its own only when none does. Each of those
  was once owed regardless, and held stdin open for a follow-up that never
  came: the turn spun until Stop and its finished answer was recorded as
  cancelled.
- **A task holds stdin from its patch to its notification.** `task_updated`
  with a terminal status comes a line before the `task_notification` that owes
  the answer. Read as the end of the wait, it closed stdin there, and the
  follow-up's permissions then failed inside Claude ("Stream closed") and a
  steer was refused. A stopped task's patch holds nothing.
- **A steer may be answered after the `result` it raced.** Once guidance has
  been written, a `result` reads to the process's exit rather than stopping it,
  or the answer to a steer reported as accepted is killed 750 ms later. Claude
  says when it still holds one: every `result` carries `queued_turn_count`,
  and stdin stays open until a later `result` reports zero. Closed at the
  first, the steer's answer ran with no way to ask for a permission.
- **Background tasks run until they end or Stop.** The CLI's
  `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` defaults to 10 minutes because a `-p`
  run has to end unattended; applied by default here, it stopped six working
  agents mid-edit. FlowX applies a ceiling only when the variable is set in the
  environment. Then it is measured from each idle `result` and again from each
  task's end, is off while a steer is being answered, and at the ceiling the
  turn completes with a note and the process group is stopped.
- **A resumed session's lost tasks are not this turn's.** Resuming a session
  whose tasks were killed opens with a `stopped` notification per task and an
  `init` plus a zero-turn `result` per batch, before the prompt is read. Those
  neither end the turn nor prove the prompt was read; only tasks started in this
  process are tracked.
- **Claude's history file holds bookkeeping FlowX never shows.** A task
  notification is a user record (`origin.kind` `task-notification`), and an
  interruption writes a `[Request interrupted by user…]` user record and a
  `<synthetic>` "No response requested." reply. The history reader drops all
  three. Kept, they split the turns around them, FlowX's copy of an
  interrupted or task-heavy thread was never found in its history, and the
  sidebar said "Thread sync needs attention" on every pass.
- **…and records a steer as something other than a user message.** A message
  sent while a turn runs is read into that turn after the current tool's
  result and recorded as an `attachment` of type `queued_command`. The reader
  keeps the user's own — `commandMode` `prompt`, not meta, origin `human` or
  none — as a user message, between the replies FlowX's live row sits between
  when the steer went in during a tool call. Read only as user records, a
  steered thread's history never caught up with FlowX's copy.

Read-only command checks for both providers go through one classifier,
`fx-agent::shell`, which fails closed on control operators, substitutions and
the write flags of otherwise read-only tools (`find -delete`, `sort -o`,
`git diff --output`).
`LineFramer` remembers how far it has scanned. Without that, a line arriving in
many chunks is rescanned from the front each time — quadratic, which for a 64 MB
message is billions of comparisons. The `scan_offset` field in the Swift original
existed for the same reason, and its test asserts a time bound rather than only
correctness.

A Codex request names its thread and turn, and one that is not the running
turn's is refused — unless a sub-agent of this thread sent it. A sub-agent is a
thread of its own, so its approvals carry its own ids; refused as foreign, every
command it asked to run failed. Codex names each child as it starts it (a
`subAgentActivity` item's `agentThreadId`, a collab call's `receiverThreadIds`,
`thread/started`'s `parentThreadId`), and `SpawnedThreads` in `codex/turn.rs`
accepts only a thread named that way; nothing is guessed. A request that is
refused still gets a row: the call it asked about, with the refusal as its
result, because its own item never reaches this transcript and a silent
refusal failed the call with nothing on screen to say why.

## The transcript

The transcript carries tool calls as first-class rows, not only as the activity
strip above it. Five rules, each of which is a bug that was written first:

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
  first result. It tracks completion for the spinner; that is all — so only
  the result claims it. Claiming the call as it started made every running call
  read as finished.
- **A steer lands where the provider reads it.** Codex takes one when it
  accepts it, so its row goes in then. Claude reads one at its next request,
  reported as `status: requesting`, so the row is held (`hold_steer`), drawn
  after the live reply, and placed at that frame or when the turn ends.
  Placed when it was written, a steer sent mid-sentence split the sentence
  around itself, and Claude's history, which keeps the sentence whole and the
  steer after the call it led to, never matched that thread again. Both fakes
  send `requesting` at every turn that asks the model anything, as the CLI
  does, and never in a zero-turn acknowledgement.

Tool output is read from `aggregatedOutput` first, then `output`, `result`,
`error`, `contentItems`, `changes`, `results`, `agentsStates` — that order is the
server's, and live turns and restored history share one reader
(`codex::session::tool_outcome`). Reading only `output`/`result` yields an empty
result for every real tool call, which then looks trivial and is dropped, so the
transcript silently loses every tool output. An explicit `null` is not output:
Codex sends `"error": null` on every successful MCP call.

Which items become rows at all is one decision too, `codex::session::item_kind`,
shared the same way. History once kept a list of its own and wrote every item
the live turn skipped — sub-agent activity, hook prompts, review markers — as a
System row of raw JSON, and each compaction as a row the live turn never had, so
a thread that used sub-agents came back after a restart as hundreds of blobs.
Plan mode's proposed plan is an item of its own, and both paths show it as
prose.

Whether a row is prose or a collapsed activity is a *view* decision, made by
`TranscriptActivityGroupingPolicy` in `fx-core`. It reads content only, never
role; the caller excludes user messages before asking it to segment.

A running turn shows its progress in three places, all read from the
conversation rather than kept by the view:

- **The live group's step.** Under the header, open or closed: the latest
  unanswered call with its elapsed time, or "Thinking…" / "Writing…" between
  calls. A call starts at its message's timestamp; the turn and the stretches
  between calls start at `turn_started_at` and `step_started_at`.
- **One tick a second**, from the executor's timer, aligned to the turn's own
  seconds. A tick changes no row's stamp and the clocks sit on fixed-height
  lines, so it re-measures nothing.
- **The plan.** `fx-core::policy::TurnPlan` reads Claude's `TodoWrite` and
  Codex's `update_plan` alike. Codex reports the latter only as
  `turn/plan/updated`, so the conversation records it as the `update_plan`
  call it stands for, and one checklist card per turn follows the turn's last
  burst of work. Plan calls are not rows or counted actions.

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

Every key binding, the menu bar, and Settings' Shortcuts page come from one
table, `commands::key_bindings()`. Add a shortcut there, never with a separate
`bind_keys` call, or the page and the menus stop telling the truth. A menu item
is enabled exactly when something on the focused path handles its action, so
commands that need a project or an idle thread register their handlers only
then.

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
- **IME composition is a marked range, not committed text.** While one is
  marked, GPUI routes keys to the input context first, so Return confirms the
  composition rather than submitting. Text with no replacement range replaces
  the marked range. Undo groups never split a composition.

Five more, each of which was a bug first:

- **A character is a grapheme cluster.** Arrows, Backspace, Delete, word
  movement and the column a vertical move keeps all step by
  `unicode-segmentation` clusters. By code point they stopped inside a flag, a
  skin tone, a ZWJ family or a letter with a combining accent, and deleting
  left half of one behind. Offsets are still UTF-8 bytes inside the field and
  UTF-16 at the `InputHandler`; only the stepping changed.
- **Line breaks are normalised in `edit`.** GPUI shapes lines only at `\n`, so a
  kept `\r` is an invisible character that costs a keypress and a Backspace
  with no visible effect. A multi-line field turns `\r\n` and `\r` into `\n`; a
  single-line one turns each break inside the text into a space and drops breaks
  at either end, so a path copied with its newline still opens.
- **A paste the cap cuts short says so.** `edit` returns how many characters
  `max_len` left out, `paste` emits `TextFieldEvent::PasteTruncated`, and the
  composer's draft shows it as its notice. Typing at the cap stays quiet.
- **Hit-testing adds back the scroll that was painted.** Paint subtracts the
  scroll, so `index_at` adds back the offset prepaint recorded in
  `FieldGeometry::painted_scroll` — not the scroll of the layout it rebuilds
  for the click, which follows the caret as it is now rather than as drawn. The
  caret row scrolls into view by its bottom, not its top.
- **GPUI's wrapped hit-test adds the row's start column itself.**
  `WrappedLine::closest_index_for_position` takes x relative to the visual row;
  adding the column too put every click on a later row at its end. It also
  truncates `y / line_height`, so ask for a row at its middle.

### Running a turn

The runner owns a `CodexSession` for a turn's lifetime and writes events into
the conversation from a background thread, so the conversation is behind a
`Mutex` — the render path holds it only for the read, never across an await.

The runner's callback **may not capture `App`**: it fires on another thread and
`App` is neither `Send` nor reachable from there. It sends to an `mpsc` channel
instead, which a 33 ms poller drains while anything is busy. The poller
repaints only when the drain applied something or a running thread's revision
moved, so a quiet turn costs no frames, and a finished turn's `Finished` update
is what repaints it without anything asking.

Two things stay off the UI thread. Transcripts are saved by one writer thread
(`conversation_writer.rs`): at most one waiting save per thread, a newer save
replaces it, and an unchanged file is not rewritten. Transcripts are read in
the background on first view (`request_conversation`); a render never reads
one, and the centre column shows a loading state until it lands. Tests that
read the store right after a save call `flush_saves()` first.

`DenyUnlessReadOnly` only bites when the thread is opened at a level that asks:
`AgentAccess::FullAccess` maps to `("never", "danger-full-access")`, so Codex
never asks and FlowX never gets to refuse. The access level has to be
`Supervised` or `AcceptEdits` for the policy to mean anything.

### Scrolling

GPUI 0.2.2's scrollable elements have **no keyboard handling at all**. The
Swift original got arrows and page keys from `NSScrollView` for free, so the
transcript adds `.track_focus()` plus an `on_key_down` that pages the list —
claiming only the keys it moves, and stopping propagation for those alone.

The transcript is a `gpui::list` aligned to the bottom: only the rows in view
are built, laid out and painted, and every other row keeps the height it was
last measured at. With no scroll position of its own the list shows its last
rows and stays there as they grow, which is what following the bottom is. Four
rules, each of which was a bug first:

- **Splice every row that changed.** Each pass stamps the rows and splices only
  the stretch whose stamps moved, so a delta re-measures the tail and nothing
  else. A reader scrolled into a spliced row is put back at the same offset;
  GPUI otherwise moves them to the row's top on every delta.
- **A pinned list's position is "the end", a viewport below the screen.** A
  step taken from there barely moves, so paging starts from what is on screen.
- **A row keeps its id across a sync.** The provider's copy of a turn has ids
  of its own, so native sync carries each local message's id onto the matching
  message (`carry_message_ids`), and a splice that finds none of the reader's
  rows keeps their place counted from the stretch's end. Taking the provider's
  ids sent a reader in the turn just finished to the top of the thread and
  closed every disclosure in it. The live reply is keyed per segment for the
  same reason: under one key, a selection in one reply came back over the
  next.
- **What is off screen is not layout.** Selection reads the rows it does not
  see from the messages, through `selection::Document`, which has to produce the
  keys and text the rows would draw.
  `the_document_reads_the_same_paragraphs_the_rows_draw` pins that.

The list's paint, like `track_scroll`, reaches `Window::current_view()`, which
unwraps a stack that only `View::paint` populates. That means **a transcript
cannot be painted through `VisualTestContext::draw`** — a bare element is not a
view. Mount a one-line `Render` probe and refresh instead; that is what
`paint()` in the `fx-app` tests does.

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

## The inspector

The inspector shows the working tree's diff and is the one place FlowX writes
to a repository. `inspector.rs` is the shell; its child modules are the Git
layer (`git_inspector.rs`), the shared poller (`repository_monitor.rs`), the
diff viewer (`diff_view.rs`) and the commit composer (`commit_composer.rs`).
The pure halves live in `fx-core::git`: `diff` parses, `operations` plans
commits and pushes, `revision` hashes a snapshot.

Six rules, each of which is a bug that was written first:

- **One poller.** The title-bar count and the inspector both observe the one
  `RepositoryMonitor`, which reads every five seconds and notifies only when
  the read differs. Two timers ran `git status` and both numstat passes twice.
- **A snapshot has a real revision.** `content_revision` hashes the status and
  numstat text, every changed path's metadata (nanoseconds, inode, ctime) and
  the index. A constant revision made every poll look like a change.
- **Diff lines are classified by hunk state, never by prefix.** Inside a hunk
  the `@@` counts say how many lines follow, and every one is content: a
  removed `-- comment` arrives as `--- comment`, an added `++i` as `+++i`. A
  conflicted file's combined diff (`diff --cc`) has a marker column per
  parent, and each is stripped. `git diff --cached` prints only
  `* Unmerged path <path>` for one, which opens a section of its own; read as
  metadata, it joined the previous file's section.
- **A refresh never blanks the diff.** The selection is a path, not a status
  record (its line counts change on every edit). The shown document stays
  until its replacement is parsed, which is then scrolled back to the same
  file and line. A failed load is not retried until something changes or
  Refresh is pressed, because the check runs on every render.
- **Untracked directories are not files.** A nested repository is reported as
  `?? vendor/lib/`; counting its lines always failed, which pinned "Line totals
  are partial" on forever.
- **A write outlives a project switch.** Writes in flight are held per project
  and report to the project they wrote: its draft is removed on success, and
  the banner waits until it is shown. Dropping the task on a switch cancelled
  it after git had already committed, so the message came back as a draft,
  with no banner, and Commit was enabled for a duplicate.

Every Git job runs on one shared multi-thread runtime whose workers have an
8 MiB stack. Jobs are *built* there (`spawn_git` takes a closure), because
debug-build futures overflow a dispatch worker's small stack.

Writes follow `fx-core::git::operations`: paths after `--` under
`--literal-pathspecs`, no plan ever contains a force flag, and a failure shows
git's whole combined output. A chosen-files commit is built in a temporary index
beside the repository's own: chosen paths from the working tree, a rename's
source as removed, then the user's index is reset to the commit for those
paths only. `--only` took the source from the working tree too, so after
`git mv old new` and a fresh `old`, the unchosen file was committed. Only a
rename has a source to remove: a copy (`status.renames=copies`) names one
too, and treating it as a rename committed the source as deleted. That commit
names no pathspec, so git's own "cannot do a partial commit during a merge"
never runs; FlowX refuses first, in git's words, while `MERGE_HEAD`,
`CHERRY_PICK_HEAD` or `REVERT_HEAD` resolves or any index entry is unmerged.
Unchecked, a merge became a two-parent commit of `HEAD` plus the chosen
files, and the other branch counted as merged with its changes dropped. Push
runs plain `git push` when `branch.<name>.merge` is set (it survives a gone
upstream, which is how an empty clone starts) and otherwise
`push --set-upstream -- <remote> HEAD`.

Open in Editor never hands an agent-written file to its default handler, which
for `.app` or `.command` means running it. Only a passive viewer type keeps its
viewer, only when the path itself is a regular file (a `shot.png` symlink to
`evil.command` is not), and the viewer is named with `open -b`, because a bare
`open` honours a per-file "Open With" attribute. Everything else goes to the
registered source-code editor, then `open -t`.

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
