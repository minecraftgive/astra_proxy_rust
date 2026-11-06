# Astra GPUI desktop GUI with full TUI parity

## Context
Astra's interactive client today is the ratatui TUI (`crates/astra-tui`, about 25k lines of non-test code, binaries `astra-tui` and `astra-server-tui`). The goal is a native desktop GUI built on GPUI that covers **every** TUI feature, TUI-only ones included: vim keys, the terminal picker, the marquee and the external editor.

Decisions already made:
- Move the rendering-free logic into a shared crate used by both frontends.
- Use `gpui-component` 0.9.
- Deliver full parity, server views included, in phases.

The TUI's `Ui` struct (`ui.rs:420`) mixes session state, data, key dispatch and ratatui rendering. Even `JobResult` (`ui/operations.rs:175`) carries ratatui-coupled widgets: `logs::Viewer`, `Editor` via `DocumentReady`, and `explanation::View`. Phase 0 therefore splits the models from rendering before any GUI work starts.

## Target layout
```
crates/astra-client   (new, no ratatui/crossterm, no gpui)
crates/astra-tui      (ratatui renderer over astra-client; behaviour unchanged)
crates/astra-gui      (new, gpui-component; bins astra-gui, astra-server-gui)
```

### astra-client contents (moved out of astra-tui)
- **Moved as is.** These have no rendering imports today: `protocol.rs`, `i18n.rs`, `export.rs`, `import.rs`, `clipboard.rs`, `external_editor.rs`, `log_stream.rs`, `self_update.rs`, `shell.rs`, `startup.rs`/`startup_unix.rs`, `terminals.rs`, `windows.rs` (+`windows/installation.rs`), `network_interfaces.rs`, `availability.rs`, `ui/connections.rs`, `ui/drafts.rs`, `ui/terminal_export.rs`.
- **`input::Key`.** A frontend-neutral key type (`code` + modifiers + kind), with `From<crossterm::KeyEvent>` in the TUI and `From<&gpui::Keystroke>` in the GUI. All shared key handlers take it, so vim keys and shortcuts behave identically in both frontends.
- **`commands`.** The `Id`/`Scope`/`Spec` table (`ui/commands.rs:52`), `command_available`, `run_command → Input`, and palette entries with disabled reasons (`ui/shell.rs:481`), with shortcuts expressed as `Key` chords.
- **`state::Client`.** The rendering-free core of `Ui`:
  - `Session` (persisted, `ui.rs:239`), rows/details and the runtime snapshot, rates history, tasks (`background.rs:31 Tasks`), `Preferences` (`ui/settings.rs:125`), installation, program info and update status.
  - The `Prompt`/`Operation`/`Input` enums.
  - `apply_operation` (`ui/operations.rs:567`) and the `ApplyContext` staleness checks.
  - Render-time `Cell`s (`profile_viewport`, `hits`, …) stay in the TUI wrapper `Ui { client: Client, …tui view state }`.
- **`operations`.** `OperationInput::capture(&Client)`, `perform`, and `JobResult`, where `JobResult` holds model types only.
- **`jobs` + `feeds`.** `Jobs` (queue of at most 8, one save per config, progress through `astra_ipc::progress::scope`) and `Feeds` (WatchList / WatchUpdates / WatchRuntime / ProgramInfo with backoff). Wake-ups go through an injected `Waker` callback instead of the static `animation::notify()`, so each frontend wakes its own loop.
- **Widget models, split per file into `model` (shared) and `render` (TUI):**
  - `editor.rs` (3184 lines): the `Editor` state machine with modes Browse, Field, Header, NewOutbound, Members, Delete and Search, plus folds, marks, undo, search and key handling.
  - `logs.rs` `Viewer`: buffer, search, filter, levels, memory limit, wrap, follow.
  - `diagnostics.rs` View; `nettools.rs` form.
  - `explanation.rs`; `ui/monitor.rs`, `ui/update.rs` and `ui/installation.rs` state.
  - `highlight.rs`: emits semantic token kinds, and each frontend maps them to colours.
- **`bootstrap`.** The `entry.rs` pieces any frontend needs: CLI parsing (`entry.rs:309-420`), finding or spawning the manager and polling until it answers (`~620-670`), `StoragePaths`, and the `update`/`update-status`/`rollback` subcommands.

Each moved module takes its tests with it (`src/test/*_test.rs` keeps the same `#[path]` pattern). The rendering-bound tests (`ui_*_test.rs`) stay in astra-tui.

### astra-gui architecture
- **Dependencies.** `gpui-component = "=0.9.0"`, using GPUI through its re-export (`gpui-pre 0.3.11`); do not also add crates.io `gpui 0.2.2`. Also astra-client, astra-app, astra-model and tokio.
- **Backend thread.** `main` starts a dedicated OS thread running a tokio runtime and a `LocalSet`, because operation futures are `!Send`. It hosts `Jobs` and `Feeds`. Traffic between the threads:
  - `BackendCmd` in: `Start(OperationInput, Action, origin)`, `Feed(…)`, `Cancel`.
  - `BackendEvent` out: `Progress`, `JobDone(JobResult, ApplyContext)`, `Rows`, `Runtime`, `Updates`, `ProgramInfo`, `Disconnected`.
  - The channels are `async-channel`, which both executors can await.
- **GUI thread.**
  - The app root is one `Entity<AppModel>` that owns `astra_client::state::Client`.
  - A `cx.spawn` loop drains `BackendEvent`s, applies them with `Client::apply_operation` and friends, then calls `cx.notify()`.
  - Views read from the model and dispatch `Action`s or `commands::Id`.
- **Keyboard.**
  - Every `commands::SPECS` entry becomes a GPUI action bound in a keymap generated from the spec table: F1–F9, Ctrl-K, Ctrl-P, Ctrl-S, Alt+1–7, 1–7, and so on.
  - Focused editor, log and list views forward `Keystroke → Key` into the shared model handlers, which keeps vim mode and the editor's `zM`/`zR`/`v`/`K`/`J` bindings.
- **Elevation** (TUN on Linux/macOS). The GUI has no terminal for `sudo`. It keeps the same flow (`PrepareElevation`, then `unix_handoff::inspect_program` and `ensure_root_controlled`, as in `ui.rs:2355 elevate`) and runs `sudo -A` with `SUDO_ASKPASS` set to `astra-gui --askpass`, which shows a small GPUI password window and prints the password to stdout. Windows keeps the existing UAC path (`windows/installation.rs`).
- **Windows.** "New window" / Alt+1–7 open real GPUI windows sharing the same model. The TUI's per-process `windows::Launch` is not needed in the GUI.

## Page and feature mapping (parity checklist)
| TUI | GUI implementation |
|---|---|
| Shell frame: status bar, page bar, hint line, task line | Sidebar (7 pages), top status bar (profile ▸ connect toggle, mode, exit chain, rates), bottom task strip (opens Tasks, F6) |
| 1 Overview (`ui/dashboard.rs`) | Header card, alerts, mode segmented control, System proxy/TUN/LAN switches, live connections with sparkline, groups, expandable traffic panel, pause |
| 2 Proxies (`live.rs:1330`) | Group list plus member grid with delay chips; test group; find |
| 3 Connections (`live.rs:1512`) | gpui-component `Table` (virtualized), grouped by destination, filter, detail pane, close one / close all (confirm dialog), pause |
| 4 Rules (`ui/pages.rs:8`) | Rule core status card, test-route dialog showing the explanation view, edit and search buttons |
| 5 Logs (`logs.rs`) | `uniform_list` virtualized viewer with source switch, search n/N, filter, display/capture level, memory limit, wrap, follow, reconnect, horizontal scroll |
| 6 Profiles (`ui/pages.rs:192`) | Proxy/Rule/Server tabs, instance table, details, and every action (n/c/s/x/r/e/L/t/b/d/y/E/P/v) as toolbar buttons and a context menu (operations menu incl. Force start/Restart) |
| 7 Settings (`ui/settings.rs`) | Program info, preferences (language, accent, colour depth used as theme intensity, motion, vim keys, mouse, start page), startup (terminal picker that launches the TUI, start at login for GUI or TUI, silent, Windows privilege service), installation, hidden marquee (Ctrl+Home held 3 s) as an animated GPUI banner |
| `Prompt` modals (AddName…Find, `ui.rs:63`) | One gpui-component `Dialog` per prompt; file pickers use `cx.prompt_for_paths` |
| Command palette, profile chooser, help F1 | Palette and chooser as searchable popovers over `palette_entries`; help sheet built from `SPECS` |
| Tasks F6, TUI log F7, Monitor F9 | Drawers/windows: task list with events; log viewer on `@tui`/`@gui`; monitor charts (pause, clear) |
| Diagnostics, Net tools | Checklist runner with streaming results / abort; Net tools form (egress, kind, target, record, server, count) |
| Updates (`ui/update.rs`, `self_update.rs`) | Updates panel: import package, status, activate (confirm), rollback, recover; the GUI restarts itself after an update |
| Editor (`editor.rs`) | Left: schema tree (folds, marks, reorder, rename, duplicate, delete with undo). Right: form rows from `astra_app::schema::rows_for`/`server_rows` using Input/Select/Switch, member picker, new-outbound wizard. Tab for raw TOML (gpui-component code editor plus shared highlight). Drafts, conflict banner, compare with disk, Ctrl-S save, import (draft/nodes/rules), export TOML/share links to a file or the clipboard, external editor |
| Recovery screen | Full-window recovery view (back / continue) |
| Server kind (`ui/server.rs`) | `astra-server-gui` (or `--core server`) opens on Profiles; server header and stats card; c/r/e/L/v actions; live pages hidden |
| CLI flags (`entry.rs:309`) | Same flags parsed by `astra_client::bootstrap`; `--view` picks the start page; `--silent` starts without a window (tray left for later) |

## Phases
0. **Extract astra-client.** Move modules, introduce `Key`, split the widget models from rendering, inject `Waker`, and make the TUI a thin renderer. Exit criterion: `cargo test -p astra-tui` and `cargo test -p astra-client` green with no TUI behaviour change. This is the largest and riskiest step, so do it in several commits, one module group at a time.
1. **GUI skeleton.** Crate, bins, gpui-component theme from `Preferences`, bootstrap/manager spawn, backend thread and event bridge, sidebar, status/task bars, palette, keymap from SPECS.
2. **Live pages.** Overview, Proxies, Connections, Rules plus the Explain dialog, Profiles plus all prompts/dialogs and the operations menu, recovery view.
3. **Observability.** Logs, TUI/GUI log, Tasks, Monitor, Diagnostics, Net tools.
4. **Editor.** Schema tree and form, raw TOML view, drafts/save/conflict, import/export, vim mode.
5. **Settings and system.** Program info, preferences, i18n switch, installation plus elevation (askpass/UAC), updates plus self-restart, start at login, terminal picker, extra windows, marquee, help.
6. **Server kind.** `astra-server-gui`.
7. **Build/CI.**
   - Add the `astra-gui` binaries to `.github/workflows/build.yml` and `ci.yml`.
   - Linux apt: `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev`.
   - Windows: `windows_subsystem = "windows"`.
   - macOS: add a minimal `.app` bundle step.
   - Keep the two-profile rule in root `Cargo.toml` (no custom profiles).

## Critical files
- New: `crates/astra-client/**`, `crates/astra-gui/**`, root `Cargo.toml` (members).
- Refactored: `crates/astra-tui/src/{lib.rs,ui.rs,ui/operations.rs,ui/background.rs,ui/commands.rs,ui/shell.rs,editor.rs,logs.rs,diagnostics.rs,nettools.rs,explanation.rs,highlight.rs,entry.rs}`.
- Reused unchanged: `astra-app` (`document.rs`, `edit.rs`, `schema.rs`, `import.rs`, `history.rs`), `astra-management::{ManagerClient, v2, wire}`, `astra-ipc::progress`, `astra-platform::unix_handoff`.

## Implementer notes (for the agent building the framework)
- **Scope of "framework".** Phase 0 complete and Phase 1 complete. Phases 2–6 as page/view stubs that compile, are wired to the model and the command table, and list their TODO features from the parity table.
- **Commits.** One commit per module group in Phase 0, each one compiling with tests green. No drive-by behaviour changes in the TUI.
- **Code style.** Follow the existing style: `src/test/*_test.rs` included through `#[path]`, `t()`/`tr!` for every user-visible string, and no new `[profile.*]` sections.
- **Pinned versions.** Do not upgrade existing dependency versions. Pin `gpui-component = "=0.9.0"`.
- **Async boundary.** Never block the GPUI thread on tokio. All manager I/O goes through the backend thread.
- **Elevation.** Keep the security checks exactly as they are: `ensure_root_controlled` and `inspect_program`.

## Review checklist (what I will check)
1. `astra-client` has zero `ratatui`/`crossterm`/`gpui` dependencies (`cargo tree -p astra-client`).
2. TUI behaviour is unchanged:
   - Every pre-existing astra-tui test still passes, apart from the known diagnostics-cancel failure.
   - No test was deleted or weakened, only moved.
3. `JobResult` staleness checks (`ApplyContext`) are preserved in both frontends.
4. Backend thread:
   - No `!Send` futures leak to GPUI.
   - Channels close cleanly on quit.
   - Manager disconnects are surfaced in the UI.
5. Keymap and palette are generated from `SPECS`, not duplicated.
6. Elevation still calls the root-controlled sudo check, and askpass never logs or persists the password.
7. Every parity-table row has a page, view or explicit TODO stub.
8. CI workflow changes are correct for all three OSes.

## Verification
- After each Phase 0 commit: `cargo test -p astra-client -p astra-tui`, plus the live suites (`ui_live_test`, `entry_test`). Known caveats:
  - The live diagnostics cancel test already fails before this work; it is not a regression.
  - `runtime_controls` tests need a short `TMPDIR` on macOS and can leave the system proxy pointing at a dead port. Check it after the run.
- Run the TUI by hand (`cargo run -p astra-tui`) to confirm it is visually unchanged.
- GUI unit tests use GPUI's `TestAppContext`:
  - The bridge applies `JobResult`s and drops stale ones.
  - The keymap matches SPECS.
  - Editor key forwarding.
- GUI end to end:
  1. Start a test manager from the fixtures used by `ui_live_test`, then `cargo run -p astra-gui`.
  2. Walk the parity table row by row: connect/disconnect, switch mode, select a proxy, close connections, test a route, follow logs, edit and save a profile (including a conflict), import and export nodes, run diagnostics and net tools, check the update status, and change language and accent.
  3. Repeat with `astra-server-gui`.
- CI matrix builds on Linux, Windows and macOS (arm64 and x86_64).
