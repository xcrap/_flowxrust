# FlowX for macOS

FlowX is a native macOS workspace for AI conversations and project inspection,
written in Rust with [GPUI](https://gpui.rs). It is an ongoing port of the Swift
FlowX application in the sibling `_flowx` repository.

The current milestone connects the application shell, provider sessions,
persistence, settings and a read-only repository inspector. **The Swift port is
not complete.** The remaining work is listed below.

## Current milestone

- Native three-column workspace with projects, threads, shared menus, keyboard
  shortcuts, theme controls and correctly embedded SVG icons. A single title
  bar, inset sidebar rows and a resizable inspector preserve room for the chat.
- Unified composer with inline configuration menus, native SF Rounded and
  SF Mono typography, compact settings/dialogs and consistent light/dark surfaces.
- Codex and Claude turns using the user's installed command-line tools, with
  streaming output, cancellation, provider session continuation and independent
  running turns in different threads.
- Opening a project discovers its Codex and Claude threads, including archived
  threads. Selecting one loads its native history; the active project index and
  selected conversation stay synchronized with external changes. Cached history
  survives provider failures, and drafts and running turns are protected.
- Thread-level provider, model, effort and access configuration. The
  provider is fixed after a conversation begins, so an existing session is not
  accidentally resumed through another provider. Model choices and supported
  reasoning levels come from the installed providers; the model picker includes
  a Refresh models action.
- Thread actions through right-click or the row menu: rename, archive and
  confirmed local deletion. Archived chats stay out of the sidebar. Search and
  restore them from **Settings → Archived chats → Manage**; their history and
  drafts are preserved. These actions manage FlowX's saved records locally.
- Interactive tool approvals and grouped questions, including masked secret
  answers. Responses return to the thread that requested them.
- Assistant replies render Markdown links, inline code, emphasis, lists and code
  blocks. Imported image attachments show bounded thumbnails; recognized provider
  attachment envelopes are replaced by the prompt and its attachments.
- Compact activity groups show progress and action counts without flooding the
  transcript. Open a group to inspect commands, with earlier actions paged in.
- Automatic names for new chats from the first prompt, using one shared provider
  and lightweight model for every chat (Codex GPT-5.6-Luna by default).
  **Settings → Chat defaults → Chat names** changes the provider and model or
  turns naming off. Each name uses one additional provider request; manual names
  and imported chats are kept.
- Saved transcripts, provider session IDs, token/context totals, drafts and
  unsent prompts. Drafts clear after provider acceptance; a failed submission
  remains retryable. Recovery copies are bounded on read, and unreadable or
  unopened conversations are preserved.
- Settings preview base tone, accent, text size and appearance immediately across
  the window. Done saves the choices; Cancel restores the previous appearance.
  Provider runtime checks report failures.
- A read-only Changes inspector shows only changed files, with staged/unstaged
  diffs, new-file previews and refresh. Reads are bounded, and preview paths are
  checked against the project root.
- A compact title-bar indicator shows green added-line and red removed-line
  counts and toggles the Changes inspector. It disappears for clean working
  trees and refreshes every five seconds and after a turn. Binary-only changes
  and incomplete line counts show a file count instead. Changes include the
  whole project; FlowX does not automatically commit, fetch, or push.

The test suite covers pure policies, storage recovery, GPUI views and provider
process behavior through local fake servers. It also exercises saving a session
to disk, reopening it and resuming its provider thread. These checks complement
manual testing in the native macOS window.

## Build and run

Requirements:

- macOS 14 or later with Metal support and the Xcode command-line tools.
- Rust **1.98.1**, pinned in `rust-toolchain.toml`, with Cargo available on `PATH`.
- Python 3 for the provider process test fixtures.
- Your own authenticated Codex CLI or Claude Code installation to run live
  conversations. Neither provider is bundled.

The workspace uses the published GPUI **0.2.2** release. Its API is the reference
for this code; current online GPUI documentation may describe a different API.

```sh
./fx build       # compile the workspace and test targets
./fx run         # run directly from the checkout
./fx launch      # build and open the development app bundle
./fx test        # run workspace tests
./fx lint        # formatting check and Clippy with warnings denied
./fx bench       # run benchmarks
./fx clean       # remove build output
```

`./fx launch` creates `target/debug/bundle/FlowX-Dev.app`. A bundle provides the
normal macOS application identity, Dock presence and menu bar. To assemble a
release build:

```sh
FX_CONFIG=release ./fx bundle
```

This produces `target/release/bundle/FlowX.app`. The helper does not sign or
notarize distribution builds.

The Rust bundles use `com.flowx.rust` and `com.flowx.rust.dev` so macOS can
distinguish them from the Swift application during development.

Zig and the Ghostty terminal integration are deferred work; the current
workspace does not require Zig to build.

## Provider setup and local data

Install and sign in to the provider CLI separately, then check its status in
FlowX Settings. Discovery supports GUI launches with a minimal `PATH`, resolves
symlinks and respects macOS quarantine. Starting FlowX does not bypass the
provider's authentication or quarantine checks.

Debug and release builds use separate application-support directories. The
Rust application discovers provider threads for projects you open: Codex through
its local app-server API, and Claude through workspace-matched local session logs
and desktop session archives. The active project index refreshes about every five
seconds and selected history about every two seconds, subject to provider latency.
Use **Refresh provider threads** in the project menu to retry immediately.

Threads retain their provider session IDs and settings when resumed. Claude
sessions still open in another process are protected against concurrent writes
when the installed CLI supports activity detection. Local rename/archive/delete
choices remain local; deleting an imported row suppresses rediscovery without
deleting the provider's transcript. Successful complete listings hide externally
removed threads while preserving their cache and any draft for recovery.

History reads are bounded at 128 MiB per conversation; oversized or incomplete
reads report an error and preserve cached history. FlowX does not migrate the
Swift application's project layout, settings or unsent drafts automatically.

For an isolated manual test, override the data directory when running the binary
from the shell:

```sh
FLOWX_DATA_DIR=/tmp/flowx-qa ./fx run
```

That directory holds the test workspace's projects, transcripts, drafts and
preferences across launches. Omit `FLOWX_DATA_DIR` to use the normal application
support location. `RUST_LOG` controls diagnostic logging.

## Remaining port work

| Area | Still needed |
| --- | --- |
| Terminal | Implement the `fx-term` sessions and GPUI renderer, input, resizing, lifecycle and multiple terminal panes. The planned Ghostty VT integration requires Zig. |
| Embedded browser | Choose and implement the WebKit bridge or an alternative preview approach, then restore browser split behavior. |
| Images | Composer attachment import, resizing/conversion, storage, provider submission and broader cache lifecycle. Native-history image previews are implemented. |
| Transcript | Markdown tables, syntax highlighting, text selection/copy behavior, and remaining Swift transcript interactions. |
| Swift data migration | Import the Swift application’s project layout, settings and unsent drafts. Provider-native history is discovered independently. |
| Thread management | Synchronize local rename/archive/delete actions with provider-native lifecycle APIs and complete remaining sidebar parity. |
| Composer | Complete attachment controls, follow-up queueing and steering behavior. |
| Git workspace | Build on the read-only inspector with the remaining diff navigation and editing workflows, staging, commit and push UI. |
| Distribution and parity | Finish workspace geometry/shortcut parity, application packaging, signing/notarization and broader live-provider acceptance testing. |

## Workspace structure

| Crate | Responsibility |
| --- | --- |
| `fx-core` | Domain models, pure policies, Git parsing and runtime discovery; no GPUI dependency. |
| `fx-design` | Theme tokens, typography, icons and shared components; no `fx-core` dependency. |
| `fx-agent` | Providers, transport, conversation state and streaming. |
| `fx-store` | Persistent formats and file storage; depends on `fx-core`, not `fx-agent`. |
| `fx-term` | Reserved for the terminal implementation. |
| `fx-app` | Application state, window shell and views. |
| `flowx` | Application entry point. |

See [AGENTS.md](AGENTS.md) for the architectural rules, design-token invariants
and GPUI 0.2.2 implementation notes.

## License

The workspace is licensed under [Apache License 2.0](LICENSE), as declared in
its Cargo manifests. Vendored Lucide icons retain their upstream ISC license
and applicable Feather MIT notices; see [NOTICE](NOTICE) and the
[upstream icon license](licenses/lucide-static-1.52.0-LICENSE).
