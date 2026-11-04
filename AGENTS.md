# AGENTS.md

This file provides guidance to coding agents (Claude Code, Codex and others) when working with code in this repository. It is the one project guide: `CLAUDE.md` only points here, so record project knowledge in this file.

## Project Overview

`Fastab` is a macOS terminal autocomplete app — a fork of the Amazon Q Developer CLI. It provides IDE-style inline completions in the terminal via a native overlay window. Key identifiers:

- **App bundle ID**: `app.fastab`
- **IME bundle ID**: `app.fastab.inputmethod`
- **CLI binary**: `ftab`
- **PTY binary**: `fterm` (`fastabterm` remains a compatibility alias; Rust crate stays `fastab_term`)
- **Desktop binary**: `fastab`

## Build & Run

**Local builds are prohibited by user instruction (2026-10-09).** Do not build or compile this project on this machine, including `cargo check`, `cargo test`, `cargo clippy`, `cargo run`, direct `rustc` probes, JS/website builds, IR generation, or install scripts that build. Use remote CI for compilation and executable regression tests. Local source review, direct `rustfmt --check`, and `git diff --check` are allowed. The commands below are references for CI or other build environments, not permission to run them locally. Keep local build caches cleared.

### Rust

```bash
# Build all release binaries
cargo build --release -p fastab_desktop -p fastab_term -p fastab_cli -p fastab_input_method

# Run a specific crate in dev mode
cargo run --bin ftab -- <subcommand>
cargo run --bin fastab

# Headless completion (uses bundle/specs-ir)
cargo run --bin ftab --features engine-cli -- engine complete --buffer "git ch"

# Lint (CI enforces -D warnings)
cargo clippy --locked --workspace --color always -- -D warnings

# Format
cargo fmt

# Test a specific crate
cargo test -p <crate_name>

# Test a single test by name
cargo test -p <crate_name> <test_name>
```

### TypeScript

```bash
# Build all packages
pnpm turbo build --filter="./packages/*"

# Compile bundled Fig specs to JSON IR + extracted JS hooks
node scripts/compile-spec-ir.mjs

# Lint all packages
pnpm lint
pnpm lint:fix

# Run tests (Vitest)
pnpm test
pnpm test:ci   # with coverage
```

### Full Install (macOS)

```bash
./scripts/install.sh    # builds Rust + TS, assembles .app, installs to /Applications
./scripts/uninstall.sh  # complete removal
```

### Release

```bash
# Bump version across Cargo.toml and TS packages, then follow the printed steps
./scripts/bump-version.sh <version>   # e.g. ./scripts/bump-version.sh 2.0.18
```

The script outputs the exact next steps:

1. Add a `## v<version>` entry to both `CHANGELOG.md` (English) and `CHANGELOG.zh-CN.md` (Chinese) — keep them in sync, one entry each
2. `git add -A && git commit -m "chore: bump version to v<version>"`
3. `git tag v<version> && git push origin main --tags`

## Architecture

### Multi-Process Design

Three cooperating native processes communicate via Unix domain sockets (protobuf messages):

1. **`fastab`** (`fastab_desktop`) — Native desktop app. Owns the GPUI autocomplete overlay (`fastab_gpui` + `overlay.rs`) and the GPUI settings window (`settings_ui.rs`). Hosts the completion engine worker, system tray, Accessibility / IME plumbing, and process-wide `NSApplication` via `gpui_host.rs`.

2. **`fterm`** (`fastab_term`) — Pseudoterminal that sits between the user's shell and their terminal emulator. Intercepts keystrokes and the shell edit buffer to drive autocomplete. Built on a vendored fork of `alacritty_terminal`. Process title is rewritten to `<shell> (fterm)`.

3. **`ftab`** (`fastab_cli`) — CLI entry point. Subcommands include `setup`, `integrations`, `hook`, `settings`, `diagnostic`, `engine`, and more.

`fterm` is the canonical PTY executable; `fastabterm` remains a compatibility alias for old shell hooks. Shell startup resolves the PTY beside the invoking Fastab/ftab executable instead of searching PATH for this shorter name, while explicit `Q_TERM_PATH` overrides remain supported. New wrappers use `<shell> (fterm)`; old `<shell> (fastabterm)` entry points are retained. Update wrappers atomically and compare bounded streaming hashes, not just the version: repeated `0.0.5` releases may have different binaries, and replacing a path must preserve existing processes' mapped copies.

**`fterm` is the process that multiplies, so its grid must not cache rows it cannot reach.** One stays resident per terminal tab, which makes it the largest consumer in a normal session — seven tabs outweighed the desktop app 88 MB to 53 MB. `fastab_term` builds its `Term` with `max_scroll_limit = 1` and reads only the current prompt line down to the viewport bottom. `Storage::initialize` used a flat `MAX_CACHE_SIZE = 1000` spare-row cache, so the first scroll allocated a thousand full-width rows the grid could never index and held them for the life of the tab: **one 40x120 `Term` cost 3.6 MB of grid where it now costs 0.4 MB**, so roughly 3 MB per tab, more at wider columns (process totals are 10–17 MB). `initialize`, `grow_visible_lines` and `shrink_lines` now take that ceiling (`Grid::max_addressable_rows` — scrollback plus viewport) and clamp the cache to it. A deep scrollback still gets the full cache, which `a_deep_scrollback_still_gets_a_cache` pins, and the `max(max_rows, required)` floor must stay: the buffer has to remain long enough to index every active line whatever ceiling the caller passes.

**What a resident fterm is made of (measured 2026-10-22, 40x120, ~5 MB).** About half was the Rust heap, then under mimalloc, which tags its memory with VM tag 100 so that `footprint` and `vmmap` print it as **IOAccelerator**: on such a build that line is the heap, not GPU memory. With both screens allocated the grids cost ~52 bytes a cell; outside the alternate screen only the primary (~24 bytes a cell plus row overhead) is kept. Do not undo these:

- **fterm links only libSystem.** `crates/fastab_term/build.rs` passes `-dead_strip_dylibs`. `objc2` (via sysinfo and whoami) declares a Foundation link with zero symbols bound, and that alone loaded ~390 images instead of ~83 and ran SkyLight's initializers before `main`: 0.6 MB per tab. `build-app.sh` and CI fail if `otool -L fterm` lists anything else. `ftab` is different: it looks AppKit classes up by name (`NSWorkspace`, `NSRunningApplication`), so its framework links must stay.
- **The alternate-screen grid exists only while that screen is shown** (`Term::inactive_grid: Option`). Entry clears every cell anyway; only its DECSC cursor survives (`alt_saved_cursor`). A second full grid was ~24 bytes a cell (0.6 MB at 80x300).
- **fterm uses the system allocator, not mimalloc.** CI's A/B on 2026-10-30 (`fterm-footprint-ab.py`, five rounds each, `/bin/sh` in a fresh pty): 4193 → 2977 KiB idle and 4337 → 3009 KiB after output at 40x120, 4497 → 3169 and 4593 → 3345 KiB at 60x240, about 30% less. mimalloc held ~2 MB of heap for a tab that keeps well under 1 MB live; the installed v0.0.20 fterm showed the same 2.1 MB. Before that, fterm had already shrunk mimalloc's arenas from 1 GiB to 32 MiB because their bitmaps dirtied ~170 KB of every tab. `ftab` still uses mimalloc: it exits after each command.
- **`SizeInfo::new` clamps to `MIN_SCREEN_LINES` x `MIN_COLUMNS`.** A 0x0 tty used to panic in `cursor_cell`, and with `panic = "abort"` that closed the tab, since fterm runs in place of the shell.
- **Socket checks in `fastab_ipc::unix_socket` use `std::fs`.** `tokio::fs` sent each `stat`/`chmod` to a blocking-pool thread, so every reconnect started a thread that retired 10 s later and left its allocator pages behind.

`scripts/fterm-footprint-ab.py NAME=PATH...` compares builds in fresh ptys without a desktop app. CI on `main` runs it on the shipped `fterm` at 40x120 and 60x240 and posts the table as the `fterm footprint` notice annotation, which the public API serves without the login the job log needs.

**That one line of scrollback is load-bearing — do not take `max_scroll_limit` to 0.** `get_current_buffer` bails when `topmost_line() > cmd_cursor.line`, and `topmost_line()` is `Line(-history_size)`. With one line of history a prompt that has just scrolled off the top of the viewport is still readable; with none, that read returns `None` and the edit buffer is lost for the rest of the command.

`fterm`'s Tokio pool is two workers, not one per core. The main loop is a single `block_on`; the rest is I/O. A ten-core machine used to start ten worker threads in every tab just to sit idle. The shell's exit is a kqueue `NOTE_EXIT` registration on that runtime's reactor (`child_exit.rs` over `fastab_util::process_exit`, which `ime_watch.rs` shares), not a thread parked in `waitpid` for the life of the tab; a thread is only the fallback when the registration fails, and the PTY's EOF still ends the loop if the event is ever missed.

**`fastab_util` must not link AppKit.** `fterm` depends on it, and a single `NSWorkspace` call in `open_url` pulled AppKit + Metal + IOAccelerator into every tab (measured: `otool -L` listed AppKit, vmmap showed a 132 MB IOAccelerator mapping). URL opens go through `/usr/bin/open`. Do not put `objc2-app-kit` / `macos-utils` back on `fastab_util` to make that call in-process.

Two things measured as *not* worth doing there. Dropping `fastab_term`'s ten unused direct dependencies (`fastab_install`, `crossterm`, `serde`, `parking_lot`, …) shrank the release `fterm` by 2.8 KB, not the megabytes it looks like it should — the linker was already dead-stripping all of it, so that edit buys build time and an honest manifest, nothing else. And the completion engine is not where the desktop app's memory goes: `Registry` indexes `specs-ir` (a 22 KB `index.json`, ~734 names) and parses at most 48 spec trees under an LRU, so the ~34 MB on disk is never resident. Typed-hook and adapter catalogs load from `typed-hooks.json`; there is no hook JS cache.

The shared file-backed SQLite pool keeps capacity for four concurrent checkouts but targets one idle connection. Its 60-second idle timeout is checked by r2d2 every 30 seconds; only returned connections are retired, and the idle target may be replenished after all expired connections close. It runs one maintenance thread (`sqlite-pool-0`), not r2d2's default three. In-memory mock storage is unchanged. `fterm` calls `fastab_settings::sqlite::prefer_direct_connections()` first thing in `main` (a call after the database opened warns and debug-asserts) and gets no pool at all: each call opens its own read-write connection and closes it (`Backend::Direct`), so no maintenance thread runs and nothing wakes the tab every 30 s. One fterm stays resident per tab and only touches the database at startup and once per finished command (history insert). A direct call never creates the data directory, so an fterm left running by `uninstall.sh` cannot bring it back. Do not assume SQLite page-cache limits are fully allocated or report them as measured savings.

### IPC

- **Local IPC**: `fastab_ipc` — Unix sockets, typed via Protobuf (`proto/` → `fastab_proto`)
- **Remote IPC**: `fastab_remote_ipc` — WebSocket-based, used for SSH/remote sessions
- Protobuf `.proto` files live in `proto/`; generated Rust types in `fastab_proto`; generated TS types in `packages/api-bindings`

### Resource lifecycle invariants

The post-0.0.6 fixes and pending runtime acceptance are recorded in `docs/resource-audit-v0.0.6.md`. Parser-generated control events are drained locally through `MainLoopControl`; they must not synchronously send into the main loop's own bounded channel. Stdin reads are 16 KiB with a 64 KiB / first-byte 1 ms aggregation bound. Large raw paste events must transfer the entire backing allocation, not leave an empty shared tail in the parser. EOF and permanent errors settle pending keys before exiting. An intercepted key that arrives while ordinary input is parked behind an unsettled key write is queued behind that input as ordinary bytes rather than sent to the desktop: a failed replay would otherwise reach the shell ahead of earlier typing. The window lasts from enqueueing the key until the outbox writer has written it, so it also covers a later intercepted key in the same input batch when ordinary bytes sit between the two, and any key while the desktop has stalled.

Desktop-to-fterm IPC uses one bounded encoded outbox (256 frames / 4 MiB including in-flight); interrupted partial writes retire the connection and must never replay an uncertain Insert. Engine initialization and ClearCaches share the watchdog boundary. Its 256-job / 8 MiB estimated mailbox reserves two EndInput slots for the committed owner and running attempt; do not coalesce across a retained Complete. File icons and delayed AX activation each use one native worker slot: an uninterruptible call continues occupying that slot, and stale results cannot refill hidden caches. Diagnostics report ownership and approximate capacities, not measured physical savings. The std-only rolling log and build identity leaves are shared with the IME by source inclusion; do not pull the desktop dependency graph back into it.

### Shell Integration

`fastab_integrations` and `fastab_install` inject hooks into shell rc files (`.zshrc`, `.bashrc`, fish config). These hooks report shell state (CWD, command text, cursor position) back to `fastab_term` via IPC on every prompt and keystroke.

`ftab init` stands down entirely when the desktop app is not running (`suppress_without_desktop_app`), so VS Code Terminal Suggest, Otty and friends keep their own completions. This is a point-in-time decision made when the shell starts: a terminal opened while the app is down remains uninstrumented for that session. After launching Fastab, open a new terminal to enable its completions.

Exports and aliases reach fterm through `ftab _ pre-cmd` (`UpdateShellContext`), a 14 MB AppKit-linked process. The zsh, bash and fish post scripts start it only when the exports or aliases changed since the last prompt (`fig_update_shell_context` / `__fig_update_shell_context`), including once at source time, so an unchanged first prompt starts nothing. zsh reads `$parameters` and `$aliases` from zsh/parameter and forks nothing; bash pays one `$(export -p; alias)` subshell, no exec; fish stays in-process. `_` is left out of zsh's fingerprint because it changes with every command. Nushell still runs `pre-cmd` synchronously at every prompt: its hooks have never run in a real nushell, so it was left alone. The rendered scripts are pinned by `crates/fastab_cli/tests/snapshots/init__*_post_*.snap`; edit both together. `scripts/shell-startup-cost.py` times what `ftab init` adds to a zsh start.

Each zsh/bash rc file runs `ftab` once, not twice. `ftab init <shell> pre` also emits the matching post script, single-quoted, as `__fastab_post_<rcfile>` (`post_handoff_variable`), and the macOS post shim evaluates and unsets it, calling `ftab init … post` only when it is unset. Pre leaves it unset when post owes the input method prompt: only the post run may count that prompt, and a pre run can belong to the shell that execs into fterm and never reaches post. With the desktop app down, pre hands over an empty script, so the stand-down also costs one launch. CI measured `ftab` at 10–15 ms per launch (about 5 ms over an empty C program; Carbon and AppKit are ~1.4 ms and ~2.2 ms of it), against a ~38 ms integration cost per zsh start. The pre snapshots (`init__*_pre_*.snap`) therefore embed the post script; regenerate both together. Fish and Nushell shims still run post themselves.

### macOS Input Method (IME)

`fastab_input_method` is an IMKit helper app (`FastabInputMethod.app`) bundled inside the main `.app` at `Contents/Helpers/`. It enables cursor position tracking in terminals that bypass the standard PTY path (Ghostty, Otty, Kitty, WezTerm, Zed, Alacritty).

- The IME self-registers with TIS on startup via `TISRegisterInputSource` (requires NSApplication context)
- Integration install/uninstall is managed via `ftab integrations install input-method`
- Enabled state is stored in SQLite: `~/Library/Application Support/fastab/data.sqlite3`, table `state`, key `input-method=app.fastab.inputmethod.enabled`

**A terminal binds to the IME process, not to the input source.** macOS hands Otty / Ghostty / Kitty an IMK connection when the window is created and never re-attaches it, so killing the IME leaves every open window talking to a dead server. `TISRegisterInputSource` and `TISEnableInputSource` do not rebind those clients. Do not disable the source to chase a reconnect: `TISDisableInputSource` followed by `TISEnableInputSource` — same turn or a later one, four retries — still leaves the palette source disabled. That is what took Otty down after install (no caret, so the overlay stayed hidden). The IME only enables itself. Existing windows stay bound to the dead process; taking the source down does not rebind them and also costs every new window. A HIToolbox fallback that writes only `AppleSelectedInputSources` is the same hole: the palette looks selected, `is_installed` used to say OK, and `AppleEnabledInputSources` still omits us, so a new Otty window never gets an IMK connection. `force_enable_in_hitoolbox` writes both lists.

**The palette write lives in `fastab_hitoolbox` and must stay in-process.** Both the IME (from its own `NSApplication`) and `ftab integrations install input-method` (from a CLI with no run loop) have to repair those two lists, so a single leaf crate over CoreFoundation owns the read-modify-write and both link it. Two reasons it is not a subprocess. TIS launches the helper with a bare `PATH`, where `python3` is the Command Line Tools stub — absent on a machine that never installed them — and a palette write that fails costs the caret in every IME-only terminal. And `defaults export`/`import` rewrites the *whole* domain: `install.sh` runs the CLI install and an IME launch in the same pass, so whichever finished second dropped the other's entry. `fastab_hitoolbox` writes the two keys with `CFPreferencesSetAppValue`, synchronizes the domain before reading so a long-lived process does not merge into its own cached snapshot, and preserves every foreign entry verbatim. The only thing it ever removes is a `Non Keyboard Input Method` under the same reverse-DNS vendor prefix, i.e. this palette under an older bundle ID; an unparsable bundle ID yields an empty prefix, which must keep matching nothing rather than every palette on the machine. Its round-trip tests run against a scratch domain — never `com.apple.HIToolbox` — and skip when cfprefsd refuses a write, which is what a sandboxed runner does.

**Replace the IME process only when its code changed.** The test is the code digest, not the executable's bytes. Before 2026-10-28 both `install.sh` and `InputMethod::install` compared the executable's SHA-256, and that changed on every release even when no IME code did: the binary embedded its commit, build date, run id and version (`build_identity.rs`), every version bump moves Cargo's `-C metadata` and with it symbol hashes and generic-code order, and the signature binds an Info.plist whose version moves too. Every update therefore killed the IME and every open terminal window's IMK connection. Now the IME's `build.rs` hashes its sources, its `#[path]` includes, `fastab_hitoolbox`, the locked closure of its runtime crates (a git crate by its commit) with their workspace feature specs, the workspace `[profile]` tables, and the compiler, target and deployment target, and writes `FastabCodeDigest` into its Info.plist. Files are hashed by their path inside the workspace: until 2026-10-29 the base was spelled `manifest_dir/../..`, which `strip_prefix` never matches, so the checkout's absolute path was in the digest and a CI build and a local build of the same code replaced the IME. Features another member enables on a shared crate and the macOS SDK version are left out on purpose; following them would restart the IME for changes nobody made to it. The script also writes `FastabBuildCommit`/`Date`/`Target`/`RunId`, which `build_identity.rs` reads at runtime under the `build_identity_from_bundle` cfg instead of compiling them in. `code_identity` (Rust) and `ime_code_identity` (`install.sh`) compare `code:<digest>`, falling back to the executable's SHA-256 for bundles from before it, so the first update onto the digest still replaces the helper once. Do not compile a per-build string into the IME, and do not hash its executable to decide a restart. Identical → leave the process running and keep `Contents/Helpers` in place (`ditto` merges, so the rest of `Contents` is still cleared; it allocates a new inode per file, so the live process keeps its own mapped copy). Different → stop it, write the new file, and launch it. The replacement only enables its TIS source; it does not disable it first. `InputMethod::install` applies the same rule through the `input-method.launched-binary-sha256` state key, with `ensure_current_binary_running` as the single place that decides. A missing tracker is *not* a reason to kill — that would undo a same-hash keep — so pin the hash instead. Never pkill because TIS failed to recognise the source: from a CLI process with no `NSApplication` that check is almost always false. After SIGTERM, wait until the process is actually gone (SIGKILL if it is not) before launching, because `open` on a live bundle only activates the old process and recording the new hash against it would hide the stale helper forever. Do not add a "restart your terminal" prompt back, and do not add a second symlink-fixing pass: `install` already repoints and re-registers the symlink, and a parallel `migrate` task raced it on the same path under `~/Library/Input Methods`.

**The desktop app is not exempt from restarting.** `install.sh` wipes and re-dittos the bundle, and `Contents/Resources/specs-ir` is read lazily at completion time (`typed-hooks.json` and spec JSON), so leaving the old process up points a live app at deleted files and completions fail silently until the next launch. It holds no IMK connections worth preserving, so it is always stopped first and relaunched at the end. Its hash gates exactly one thing: the Accessibility check after relaunch, since an identical binary keeps its code-signing identity and therefore the grant the user already gave.

**Do not `tccutil reset Accessibility` on a binary change without asking the new process first.** The ad-hoc designated requirement is a bare `cdhash`, so every rebuild is a new identity to TCC — but on macOS 26 TCC re-pins the stored row itself: after two in-place swaps without any reset, `TCC.db` held the *new* binary's cdhash with `auth_reason = 4` (System Set) and `AXIsProcessTrusted()` was true from the first check. A blind reset there deletes a grant that carried over and sends the user to System Settings on every install. Older releases keep the stale requirement and leave the checkbox ticked while every AX call fails, which is the case the reset exists for. `install.sh` therefore launches the new app, runs `probe_accessibility` (`ftab debug accessibility status`, which reports the desktop process's own `AXIsProcessTrusted()` over its socket, so it speaks for the installed binary and not the shell), and resets only on a repeated `false`, then tells the user to Grant from Fastab Settings; an unreachable app gets a hint, not a reset. The app's own `reconcile_accessibility_permission` records or clears the grant and never opens System Settings; a granted → revoked transition is surfaced in the tray, and the user grants again from Fastab Settings. The permanent fix would be a stable signing identity (`SIGNING_IDENTITY` in `build-app.sh`), which pins the requirement to a certificate instead of a cdhash.

**Launch the IME with `/usr/bin/open -g`, never `NSWorkspace` from our own process.** LaunchServices records the app that asks for a launch as the new app's parent (`lsappinfo info -only parentASN` showed `"FasTab"`), and loginwindow force-quits a parent's child apps about 3 s after the parent exits (`com.apple.libquit` "force quit (caller responsible for termination)"). Every desktop quit or update therefore killed the IME, and with it every open terminal window's IMK connection. Measured 2026-10-27 on the installed app: an `open`-launched IME has no parent and was still running 6 s after `ftab quit`; one the desktop launched through `NSWorkspace.openURL` was gone. `macos-utils` no longer has a `launch_application`. An IME launched by an older desktop still carries that parent, so the first desktop quit after updating can still kill it once.

The launch-time restore is one check, and it can run while that doomed IME is still alive: it saw a live helper with the right hash, left it alone, and loginwindow killed it a second later. `crates/fastab_desktop/src/ime_watch.rs` therefore keeps watching: a kqueue `EVFILT_PROC`/`NOTE_EXIT` registration on the running IME, polled through tokio's reactor (no thread, no wakeups while it is up). When it exits and the IME is still chosen, it waits 2 s for whoever stopped it to finish and runs `restore_if_chosen` again under the same operation lock as install/uninstall, at most three times per five minutes. A relaunch only serves windows opened afterwards; windows bound to the dead process stay without a caret.

When the caret never arrives, the overlay stays hidden. There is no window-rect fallback: a list placed from the window frame lands away from the real cursor, which is worse than no list. Each IME caret frame names its client: bundle ID plus the `NSWindow.windowNumber`, which is the same `CGWindowID` AX reports. A frame queued before focus moved is dropped when the bundle ID is not the frontmost app, or when the window belongs to the frontmost app, differs from the tracked focused window, and AX still confirms that tracked window. With no tracked window, or one AX no longer confirms, the frame is placed, so a missed focus notification cannot hide the list for good. Frames from an older IME carry neither field and keep the origin and frontmost-terminal checks.

**`fastab_input_method` deliberately links almost nothing.** It stays resident for as long as a terminal holds an IMK connection, so it must not pull the desktop stack: `fastab_ipc`, `fastab_proto`, `fastab_log`, `fastab_util` and `macos-utils` between them drag in tokio, prost, tracing-subscriber, regex and sysinfo, which cost ~9 MB of footprint for a process whose whole job is one caret query per keystroke. The four pieces it actually needs are reimplemented locally and pinned to their originals by tests that take `fastab_proto` / `fastab_util` as **dev**-dependencies: `paths.rs` (socket + log paths vs `fastab_util::directories`), `wire.rs` (the caret frame, byte-for-byte vs the prost encoder), `terminals.rs` (bundle IDs vs `Terminal::supports_macos_input_method`) and `logging.rs` (a file and a `Q_LOG_LEVEL` level). Add a dependency here only after checking what it costs; if you need something from `fastab_util`, copy it and extend the equivalence test. `fastab_hitoolbox` is the one exception, and only because it is a leaf over CoreFoundation, which this process already links for TIS.

The caret sender is one `std` thread that coalesces queued frames to the newest, skips the connect when the desktop socket is absent, and retires itself after 30 s idle. An idle IME is therefore just its AppKit main thread (~7 MB).

### Native UI

The overlay and settings window are GPUI views, not WebViews. Do not reintroduce `wry` / WKWebView for either surface.

- **Overlay** — `crates/fastab_gpui` renders the list; `crates/fastab_desktop/src/overlay.rs` owns completion requests, insertion, intercept flags, and caret placement. The window is parked with `orderOut` immediately when hidden, then removed after 10 seconds idle to release its renderer and atlas. Hidden payloads (rows, strings and PNGs) retire with the window after the grace, retaining only selected-row identity. Hidden onlyShowOnTab/Esc input does not run completion or caret refresh; explicit Show/Tab owns a fresh input/caret request and recreates the window from the new result. The global Tab entry must remain independent of retained rows and caret, including after re-enabling completion. Hidden caret updates must not create a window merely because an argument hint remains. Caret coordinates are Quartz (top-left, origin at the primary display). Convert to Cocoa with `NSScreen.screens[0]`, **not** `mainScreen` — `mainScreen` is the focused display and breaks external-monitor placement.
- **Settings** — `crates/fastab_desktop/src/settings_ui.rs`. Appearance / Behavior / About, plus a permission gate. AI Settings is temporarily hidden with `AI_SETTINGS_VISIBLE = false`; old AI routes fall back to Appearance and the AI settings Entity is not constructed. Saved runtime AI configuration and consent remain unchanged. Language key is still `dashboard.language` (`system`, `en`, `zh-CN`).
- **Host** — `crates/fastab_desktop/src/gpui_host.rs` runs one `NSApplication` for overlay + settings + tray.

The host defers application-wide UI cache retirement until the last GPUI window has actually dropped. `App::release_idle_caches` checks the internal window slots (an updating window still counts), clears decoded asset tasks and derived text caches, and preserves FontId mappings/platform font tables. Text pools use generations so borrowed wrappers and font-run buffers cannot refill a retired pool. The overlay's existing hidden 10-second grace still governs window destruction. Do not equate released cache references with physical footprint: allocator pages and native framework caches can remain resident.

**Released memory goes back to malloc, not to the system, until `heap_trim` runs.** libmalloc keeps emptied pages mapped and dirty: an app left idle for hours sat at 37 MB against 14.6 MB for a fresh launch, and the fresh launch already held ~4 MB of free heap (53% of the default zone). `crates/fastab_desktop/src/heap_trim.rs` calls `malloc_zone_pressure_relief(NULL, 0)` once, 15 s after the last engine request, EndInput or UI cache release (`install_idle_cache_release`'s callback), which is past both 10 s graces. It runs once per quiet period on a tokio worker, and a quiet app is never woken. It frees only pages with no live allocation; Metal, CoreText and AppKit caches stay. Automatic restart on idle was considered and declined: do not add it.

**The desktop stays on the system allocator; mimalloc made it worse.** Attributed on installed v0.0.19 with `MallocStackLogging`: no long-run growth (13.4 MiB live at 4 minutes and after 15 hours), ~9 MiB of first-use residue that is almost all framework caches (AppKit, CoreText on the fonts GPUI keeps, the main menu, Metal), and ~12 MB of malloc-zone fragmentation, about half the zone. v0.0.20 moved the desktop's Rust allocations to mimalloc on the theory that transient GPUI and engine blocks pinned those pages. Measured on the installed v0.0.20 it did not: with Rust gone from the zone, the zone still held 9.5 MB of fragmentation (44%), so that fragmentation is framework and C allocations; and mimalloc held ~9.4 MB dirty for ~1.5 MB of live Rust and did not shrink after `mi_collect`, which only collects the calling thread's heap while every allocating thread (main, tokio, `ec-engine`, GCD callbacks) keeps its own pages. Footprint went from ~36 to ~43 MB, so it was reverted. The v0.0.19 high-water-mark reading that suggested Rust was dominated by a ~42 MiB IOSurface peak; do not read a malloc cause from it. Do not move the desktop to mimalloc again without measuring an installed build. Repeat the attribution with `open -g --env MallocStackLogging=lite /Applications/Fastab.app`; on macOS 27 `MallocStackLogging=1` means lite, and `malloc_history -highWaterMark` needs `=full` plus `MallocStackLoggingDirectory`. Release binaries are stripped, so map Rust frames with the release's dSYM (see Binary and package size).

Synchronous AppKit queries need their own autorelease pools when called from Tokio or before the native event loop. Keep pools inside synchronous leaf functions returning owned values; the main-thread startup pool must drain before GPUI runs, and a pool must never span a migratable async future. Accessibility observers are keyed by PID plus bundle ID: replacement and termination remove only that instance, preserving other processes with the same bundle.

The Metal renderer allocates full-window path targets only for a scene containing paths. Resizing retires old targets; an unchanged size reuses them, and 4x MSAA remains enabled. This avoids unnecessary texture requests for ordinary Settings frames; it does not establish a fixed idle-footprint reduction.

**The UI loop runs outside `tokio::Runtime::block_on`, and nothing awaited on GPUI's foreground executor may be a tokio resource.** `main` does `block_on(async_main())` for the async prelude, which returns a `gpui_host::Setup` closure, then holds `runtime.enter()` and calls `start_application`. It used to call `start_application` from inside `async_main`, so `NSApplication::run` lived inside one `block_on` poll that never returned. `block_on` fixes tokio's cooperative budget — 128 units — for the life of that poll and only refills it when the poll returns. Every `tokio::sync::oneshot` reply the completion future resolved on the main thread spent one; from the 129th, `Receiver::poll` returned Pending and immediately woke itself (`coop::register_waker` → `context::defer`, which has no scheduler on that thread and falls back to `wake_by_ref`), and `async_task` re-queued the runnable with `dispatch_async_f` onto the main queue GCD was already draining. Result: 100% CPU with the overlay frozen on `···`, reproducibly after about 128 completions — hours of normal typing. `runtime.enter()` keeps `Handle::current()` / `tokio::spawn` working from AppKit callbacks without setting a budget. Independently, `EngineClient::complete` replies over `futures::channel::oneshot`, which has no budget accounting; `replies_keep_resolving_after_tokio_exhausts_its_budget_on_the_polling_thread` drives 200 replies inside one `block_on` poll and fails with the tokio channel at round 128. If a GPUI-side future ever needs a tokio primitive, wrap it in `tokio::task::unconstrained`. The earlier `exec_after` change in `fastab_gpui/src/macos.rs` was shipped as the fix for this hang; it was not (the same-turn drain re-entry it prevents is real but was never observed), so do not read its changelog entry as the diagnosis. To reproduce without a terminal, a driver that handshakes on `remote.sock` and streams `EditBufferHook`s plus caret frames on `desktop.sock` (the framing `fastab_input_method/src/wire.rs` hand-encodes) hangs an unfixed build in under a minute.

**Every AX value that comes back from a `Copy*` or `Create*` call is +1 and must be wrapped under the create rule.** `macos-utils`' `UIElement::get_attr_ref` used `wrap_under_get_rule` on `AXUIElementCopyAttributeValue`, which retained the value a second time, so every attribute read leaked its result — and a `kAXChildren` array pins every child element with it. The xterm caret walk (`find_x_term_caret_tree`, run for VS Code / Cursor / Windsurf on each `EditBufferChanged`) reads `children()` and `role()` per element from the window root whenever the cached path misses, roughly 140 elements a keystroke; `heap` on a Cursor session showed 211k `AXUIElement` + 211k `CFData` + 112k `NSMutableArray`, 72 MB of a 131 MB footprint. `From<AXUIElementRef>` is the *borrowing* constructor (observer callback arguments, entries of a children array) and stays get-rule; owned elements come from `UIElement::application(pid)` or `AXUIElement::wrap_under_create_rule`. `AXObserver` owns its application element and releases the observer on drop — it was re-created on every app activation and never released. `caret_position.rs` owns both the range it builds with `AXValueCreate` and the `AXBoundsForRange` result. `owned_and_borrowed_constructors_balance_their_retains` checks the constructors against real retain counts (`AXUIElementCreateApplication` needs no Accessibility grant); the `Copy`-rule wraps are pinned at the source because a test binary has no AX tree to read. To measure: `heap $(pgrep -x fastab) | grep AXUIElement` before and after a few hundred keystrokes with an Electron terminal frontmost; the count must not move. Expect the failing walk itself to cost ~60 ms of main-thread time per keystroke — it is a full tree traversal — so a terminal whose caret is never found is slow as well as overlay-less.

`overlay.rs` reproduces the WebView's `HIDDEN_UNTIL_KEYPRESS` triggers: backspacing a whole token away, and a buffer change that does not look like typing (a paste or a shell history recall). Both hide the list until the next keystroke; an insertion the overlay made itself is exempt, which is what `self_insertion` tracks.

Insertion edits count shell cursor positions, not UTF-16 units or bytes: every `\x08` and `\x1b[D` moves over one character plus the zero-width marks after it (readline, fish, and zsh with `COMBINING_CHARS`, which macOS's `/etc/zshrc` sets). The WebView's JS `.length` counting deleted an extra character per emoji. `fterm` reconciles a stale `insertion_buffer` in the same units. The paste heuristic (`large_buffer_change`) still uses UTF-16 lengths on purpose.

`settings.json` is replaced by temp file + `rename` (a symlink's target, even a missing one, is written and the link kept; permissions kept; stale temp files removed; plain `fsync`, not `sync_all`'s `F_FULLFSYNC`, because Settings writes on the UI thread) and is never rewritten on a failed parse: every `fterm` loads it at tab start, so the old write-`{}`-on-error path wiped hand-edited settings. An unparsable file is copied to `settings.json.invalid-<content hash>` (newest three kept) and the process runs on defaults.

Relevant settings:

| Key                | Default | Effect                                                                                                              |
| ------------------ | ------- | ------------------------------------------------------------------------------------------------------------------- |
| `dashboard.language` | unset | Settings UI language: `system`, `en`, or `zh-CN`                                                                    |
| `app.silentLaunch` | `false` | Start without opening settings, same as `--no-dashboard`. A `fastab://` deep link naming a page or a missing Accessibility grant overrides it |
| `autocomplete.scriptTimeout` | `5000` | Per-hook script budget in ms. The overlay retires `···` after this plus 1s (floor 2s) but keeps waiting, so a late result still renders. The engine worker's wedged-thread watchdog stays at ≥30s. |

The WebView overlay and dashboard sources (`packages/autocomplete-app`, `packages/dashboard-app`) were deleted after v2.2.2 — nothing loaded them once both surfaces went native. Read them out of git when you need the legacy insertion / ranking behavior as a reference. The repo carries no tags, so use the v2.2.2 bump commit `edf0936a`: `git show edf0936a:packages/autocomplete-app/src/state/insertion.ts`, or `git worktree add /tmp/ec-baseline edf0936a` for a full tree to diff against.

The Rust half of that bridge is gone too: `fastab_desktop/src/protocol/` (the `fig://`, `ecresource://` and `spfastab://` handlers), `fastab_desktop/src/request/`, and most of `fastab_desktop_api`. `crates/fastab_desktop/src/webview/` keeps its name but is now just the app bootstrap — `WebviewManager` owns the event loop, tray and IPC, and starts GPUI. Do not restore `#![allow(dead_code)]` on those modules; it is what let the bridge rot in place unnoticed. Note that `mod` versus `pub mod` matters here: a `pub mod` at the crate root exempts its items from both the `dead_code` lint and clippy's `avoid_breaking_exported_api` lints, which hides exactly this class of leftover.

`crates/fastab_desktop/src/platform/` still carries `linux/` and `windows.rs` behind the `cfg_if!` in `platform/mod.rs`, but **neither compiles** and neither has since before the GPUI migration. `windows.rs` refers to `RelativeDirection` and `FigWindowMap`, two types that no longer exist anywhere in the crate; `linux/` needs `x11rb`, `zbus` and `dbus`, none of which are declared in `Cargo.toml`. Only the macOS branch is real, so `cargo clippy --workspace -- -D warnings` on macOS covers everything that builds. Treat those two branches as fork archaeology: reviving either means repairing it against the current `event.rs` / `webview` types, not just flipping a target.

### Completion engine

`fastab_engine` runs on a dedicated worker thread (`EngineClient`). Bundled Fig specs are compiled at build time by `scripts/compile-spec-ir.mjs` into `bundle/specs-ir/` (JSON IR + `typed-hooks.json`). `build-app.sh` copies that tree to `Contents/Resources/specs-ir/`. Override the directory with `EC_SPECS_DIR`. The desktop app does not execute JavaScript.

Most completions are pure Rust (lookup, builtins, file paths, history, ranking). A generator hook is either typed IR or a named native adapter, both loaded from `typed-hooks.json`:

| Kind | Role |
| ---- | ---- |
| typed IR | Closed expression language (`scripts/typed-hook-ir.mjs` / `crates/fastab_engine/src/typed_hook`). Helper inlining happens at compile time. |
| native adapter | `functionBodySha256`-keyed Rust in `native_adapters`, bound through the sidecar `adapters` map when the compiler cannot emit typed IR. |

| Hook | Role |
| ---- | ---- |
| `postProcess` | Script stdout → suggestion rows |
| `script` | Returns argv; Rust executes it |
| `custom` | Whole generator; may call `executeCommand` |
| `generateSpec` | Walk-time spec merged into the node |
| `getQueryTerm` | Token → query used for matching |
| `trigger` | Whether to rerun a generator |
| `filterTemplateSuggestions` | Filter file/folder/history rows |
| `alias` | Expand a token before the next walk |
| `loadSpec` | Replace the current node with a spec |

Empty `cwd` skips script and custom hooks. Results are cached (`hook_cache`: suggestions / script stdout / specs, each capped at 512 entries and 4 MiB estimated payload). Ending input starts a 10-second idle grace for hook results as well as specs; repeated EndInput, history-only and cancelled requests must not extend an existing deadline. The worker waits on the earliest hook or spec deadline, even when no spec file remains cached. A request that turns on the `···` marker owns that latch and must clear it even if its result is stale.

Every hook runs under a hard wall-clock deadline (its script budget plus a 2s margin). `executeCommand` calls are clamped to the remaining budget — a spinning or slow hook is aborted instead of wedging the attempt thread until the 30s supervisor watchdog. Watchdog timeouts/panics log at ERROR (the default log filter) with the root command and cwd.

The typed-hook catalog and adapter bindings also retire after 10 seconds without an active input owner, even when spec and hook-result caches are empty. Cold or rebound catalogs need an idle deadline before early cancellation can return. Normal completion binds a lazy owner to the current verified `Registry.snapshot()`; only actual hook dispatch parses the catalog. Unused captured sidecar bytes must be released on every request exit and on owner drop, because the pristine registry shares the snapshot. Preserve the distinction between an unloaded owner and a loaded owner whose sidecar is missing/rejected. `dump-state engine` reports catalog ownership, descriptor counts and captured byte capacity without parsing more hooks.

A dispatch parses one command's block of the catalog, not all of it. The compiler sorts hook ids (`<command>#<field>#<n>`), so each command's hooks and adapters are one contiguous run of members. The first dispatch of a snapshot generation pins `typed-hooks.json` (`DirectorySnapshot::pin_file`: read, digest-checked, descriptor kept) and records each command's byte ranges (`index_typed_hook_catalog_bytes`, which validates the header and every id). The index (a few KB plus one descriptor) lives in the snapshot's `sidecar_index` slot, so the owner the engine drops at every idle grace is rebuilt from it; a block is read with `pread` and validated entry by entry when its first hook runs. CI timed the whole catalog at ~13 ms (4.6 MB read, digest and parse) on the first hook after each idle grace; git's block is 351 KB. A pinned inode whose size or change times moved fails closed and marks the generation stale, as a digest mismatch does. An escaped key, or a command whose members are not contiguous, makes the index build fail and the owner parses the catalog whole, as before. Snapshot-less owners (tests, `from_catalog`) also parse it whole.

Opening a generation still digests every IR file (spec-pair verification and later reads compare against those digests), but `capture_tree` walks on the calling thread and hashes on up to four others, with at most 16 opened files queued (`HASH_QUEUE`). Done serially it was 126 ms of the 319 ms first completion after launch on CI.

**A spec's own script runs whenever something shapes its output.** `compile-spec-ir.mjs` substitutes a native git builtin (`git-branches`, `git-commits`, …) only for a script with neither `postProcess` nor `splitOn`, which Fig would have shown nothing for. From the GPUI migration until 2026-10-26 it substituted them unconditionally and dropped the `script`, so all 164 git/hub generators lost their upstream postProcess: `git checkout` listed `origin/foo` alphabetically from `for-each-ref`, with no current-branch row and no committer-date order, where WebView v2.2.2 and Fig ran `git branch -a --sort=-committerdate`. Those hooks are typed IR, so this runs no JavaScript. When a spec change moves a script's argv, the CLI output samples and hook baselines must follow. Push the change to a `regen/**` branch: the Regenerate hook fixtures workflow compiles IR, runs `record-cli-output.mjs --missing` (only the new samples; dropped ones are pruned, unlike `--update`, which re-records all of them on whatever machine runs it), recomputes baselines, the inventory and the typed-hook references in `testdata/typed-hooks` (each pins `pairSha256`, the hash of the bundled source + IR pair, so any compiler change that alters IR moves all six), and commits them back to that branch. `capture-hook-baseline --update` leaves a file alone when `--check` would accept it, so a timeout case another machine recorded as an immediate error stays as it was. Review that commit, then merge. The workflow has no `workflow_dispatch` (on main it would commit unreviewed) and checks out without persisted credentials; only its final push step sees the write token.

Fig semantics the Rust side has to reproduce exactly: a generator's `splitOn` wins over its `postProcess`, and `custom` hooks get the shell's process name and environment variables on their context argument (`hook_backend::enter_context`, fed from `CompleteRequest::environment_variables`). `context.searchTerm` is parser inner text, not the raw shell spelling.

A `generateSpec` runs once per command prefix, not per keystroke. Fig cached the whole parse by the finished tokens, the spec, the cwd and the shell process (`parseArgumentsCached`), so `git c`, `git ch` and `git che` ran `git help -a` once; 65 bundled specs (git, cargo, node, python, pnpm, yarn, rails, php, …) have a `generateSpec` and none carries a `generateSpecCacheKey`. `generate_spec_cache_key` reproduces that key for the hook cache (and keeps Fig's `<command>:<key>` when a spec has one). Generated specs also survive the idle grace, as Fig's parse cache lasted the session: `HookCache::release_idle` drops generator results but keeps the spec map while it is under `IDLE_SPEC_CACHE_BYTES` (256 KiB), and `has_idle_results` leaves that map out of the idle deadline, so holding it never wakes the worker. Without that, `git help -a` (24 ms on CI) and `cargo --list` (~100 ms) reran on the first keystroke of every command typed after a pause. A generation change or Clear Cache still empties it. A failed run is not cached; history parsing (`without_hooks`) neither runs the hook nor takes a cached result.

Fig ran every generator as its own promise and drew the static rows while they loaded; the engine runs them in order inside one request. Only `debounce` used to defer one. A generator that runs a command or hook and whose last run took ≥80 ms (`SLOW_GENERATOR`, a process-wide mark keyed by `generator_speed_key`, set again by a run cancelled after 80 ms and cleared by a fast finished run) now takes the same path: the request returns the static rows (plus the generator's old rows when it has no `getQueryTerm`) with `pending_generators` and `debounce_ms: Some(0)`, and the overlay's follow-up runs it. Fast generators stay inline so `git checkout ` does not paint the options a frame before the branches. Path and history templates never defer: they retrigger on most keystrokes, and with a `getQueryTerm` each follow-up would blank the list. Only callers that answer `pending_generators` may opt in, through `CompleteRequest::defer_slow_generators` (desktop only, `serde(skip)`); the CLI, golden cases and `complete_blocking` keep one request per complete result. `crates/fastab_engine/examples/cold-load.rs` times the first completion after the engine retired its caches against a warm one; CI on `main` posts it, and the shell-startup table, as notice annotations.

**Shell history comes from the shell's own file, not from a login shell.** History loads once per engine and source (`rank::load_commands_for`; the `HistorySourceConfig` key now includes the resolved files). Until 2026-11-03 the first completion after launch, and every switch between shells, ran `zsh -lc 'fc -R; fc -ln -10000'` (bash `fc -ln`, fish `history search`). A non-interactive zsh never sources `/etc/zshrc`, where macOS sets `HISTFILE`, so `fc` failed with "no such event"; a non-interactive bash keeps no history and printed nothing. Both fell back to the database after paying for a login shell and the profile it sources, Fastab's zprofile shims and their `ftab` launches included. A `HISTFILE` set in `.zshenv` was worse: zsh's non-interactive `HISTSIZE` of 30 replaced the database with 30 rows. `shell_history.rs` reads `${ZDOTDIR:-$HOME}/.zsh_history`, `~/.bash_history` and `${XDG_DATA_HOME:-~/.local/share}/fish/fish_history` instead. An exported `HISTFILE` wins for the current shell, and HOME, ZDOTDIR and XDG_DATA_HOME come from the terminal's exports first. Only the last 4 MiB is read, and the rows are what each shell's listing printed. zsh events are unmetafied and escaped as `fc -l`'s `nicezputs` does (`\n`, `\t`, `^X`), so a multi-line command stays one row, as it did through the WebView's interactive `zsh -lic`. bash lines skip `#<epoch>` stamps. fish entries are listed once, at their newest use, line by line. A missing or empty file still falls back to that shell's database rows. A `HISTFILE` assigned in `.zshrc` without `export` is invisible to the engine, which reads the default path. Do not put a shell spawn back: `zsh -lic` would be faithful, but it runs the user's whole `.zshrc` on the completion path, and the `shell-*` engine golden cases pin zero process calls (their `expectCalls: 1` was this spawn). `cold-load.rs` sends its requests from zsh with HOME on a scratch directory holding a 10,000-command `.zsh_history`, and times the former `zsh -lc` source on the runner beside them.

`packages/autocomplete-engine` is a TypeScript experiment and is not on the desktop path.

### Website Tailwind CSS

The product website under `website/src` uses Tailwind CSS v4. When editing it:

- Prefer canonical utilities and do not leave `suggestCanonicalClasses` warnings in edited files.
- Use the spacing scale for exact equivalents: `py-3.5` instead of `py-[14px]`, `gap-6.5` instead of `gap-[26px]`, and `max-w-310` instead of `max-w-[1240px]`.
- Use Tailwind v4 CSS-variable shorthand: `bg-(--accent)`, `text-(--accent)`, `border-(--accent-line)`, and `font-(--font-display)` instead of `[var(...)]` forms.
- Prefer exact named utilities such as `rounded-md`, `rounded-xl`, and `tracking-wider`.
- Use arbitrary values only when no canonical utility expresses the design, such as a custom `11px` radius or a complex shadow.
- Before finishing website changes, clear Tailwind CSS IntelliSense canonical-class warnings in touched files and run `cd website && pnpm build`.

### Bundled Specs

Completion specs are **bundled into the `.app` at build time**, not fetched at runtime. `scripts/sync-bundled-specs.mjs` assembles the Fig JS sources into `bundle/specs/`, records an exact package/lock/config/tree freshness manifest, compiles and audits the replacement IR, and atomically publishes the source/IR pair. `--check` is the only read-only mode; every successful sync publishes both trees. Standalone custom outputs must set both `BUNDLED_SPECS_DIR` and `EC_SPECS_IR` to distinct custom paths; a canonical source with custom IR (or the reverse) is rejected so the canonical source/IR pair remains isolated. `scripts/compile-spec-ir.mjs` also supports an independent, atomic IR rebuild. `build-app.sh` checks source freshness, publishes matching IR before Rust starts, then holds the pair lock across rustc's `include_bytes!` reads, exact Cargo-JSON binary snapshots, and an audited IR snapshot. The assembled bundle receives only those locked snapshots, and publication either renames it into an absent final path or uses macOS `RENAME_SWAP` under a publication lock, so an older complete app remains available until a newer complete app replaces it atomically. Its embedded icons and lazy runtime spec data therefore come from one generation. It ships **only `specs-ir`** in `Contents/Resources/`. `bundle/specs` stays out of the `.app`: it exists to feed the IR compiler and to supply the icons `fastab_gpui` embeds with `include_bytes!`, both build-time concerns. Bundling it too cost 28 MB of dead weight until it was dropped. The engine reads **`specs-ir` only** — a spec missing from that tree has no completion. Runtime hooks live in `typed-hooks.json` (typed IR plus an `adapters` map). `hooks/`, `source-modules/`, and `hook-modules.json` must not appear; compile, audit, spec-pair, `build-app.sh`, and CI `scripts/assert-no-runtime-js.sh` fail if they do. Node is build-time only (compiler, capture, classify). Versioned specs compile into per-version IR files listed on `index.json`'s `versioned` map. Coverage numbers live in `crates/fastab_engine/testdata/native-hooks/inventory.json` (`node scripts/classify-native-hooks.mjs --check|--update`). Unknown function fields still fail the compile.

**Source.** The default source is the installed npm dependency [`@chen86860/autocomplete-specs`](https://www.npmjs.com/package/@chen86860/autocomplete-specs), published from our forked spec repo [`chen86860/autocomplete-specs`](https://github.com/chen86860/autocomplete-specs). The version is pinned by root `package.json` plus `pnpm-lock.yaml`. The sync script reads the package from `node_modules`, copies `build/*.js` and `icons/*.png` into `bundle/specs`, then derives `index.json` from the bundled file tree.

**Config + pinning.** `specs.config.json` only stores bundle filtering such as `exclude`. Its bytes are hashed into `bundle/specs/.source-manifest.json` (`inputs.specsConfigSha256`), so any edit to it, even its `$comment`, needs a re-sync; otherwise `sync-bundled-specs.mjs --check` fails CI's Verify bundled specs source step. Package pinning lives in the normal JS dependency files, **not `latest`**, so the bundle changes only when the dependency changes, never silently. To adopt a newer fork build: run `corepack pnpm add -D @chen86860/autocomplete-specs@<version> -w`, re-run the sync, and commit `package.json`, `pnpm-lock.yaml`, and the regenerated `bundle/specs` together. Env overrides still win for one-off runs: `BUNDLED_SPECS_EXCLUDE=<csv>`, `BUNDLED_SPECS_PACKAGE=<pkg>`, `BUNDLED_SPECS_SOURCE=npm` with `BUNDLED_SPECS_VERSION=<version|latest>` / `BUNDLED_SPECS_PACKAGE_TARBALL=<full-url>` / `BUNDLED_SPECS_NPM_REGISTRY=<registry>`, or `BUNDLED_SPECS_SOURCE=cdn` to fall back to the legacy per-file CDN sync (`https://specs.q.us-east-1.amazonaws.com/`, frozen 2025-05-05).

To keep the bundle small, the sync script supports excluding whole namespaces via `BUNDLED_SPECS_EXCLUDE` (comma-separated; a namespace `ns` drops the top-level `ns` spec and everything under `ns/`). The filter is applied to **both** the downloaded files and the written `index.json`, so the runtime loader never references excluded specs.

- **Default**: `aws` and `az` are excluded (the AWS and Azure CLI specs are large and most users never trigger them). This is intentional — see `specs.config.json`.
- **Bundle everything**: `BUNDLED_SPECS_EXCLUDE="" node scripts/sync-bundled-specs.mjs`
- **Trim more**: `BUNDLED_SPECS_EXCLUDE="aws,gcloud,az" node scripts/sync-bundled-specs.mjs` (saves another ~26 MB)

`build-app.sh` only auto-syncs when `bundle/specs/index.json` is missing, so it reuses whatever filtered set is already on disk. Re-run the sync script after changing the exclusion list.

### Binary and package size

The DMG is ULMO (LZMA), not create-dmg's default UDZO: compressed, the bundle is mostly Mach-O code, and LZMA was estimated at ~17 MB against 23.6 MB. `make-dmg.sh` fails if the image it produced is any other format. Spec JSON compresses ~11x inside the DMG, so trimming specs barely moves the download; it moves installed size. Do not compress `specs-ir` on disk to save installed space: the user ruled out decompressing at load time.

Shipped binaries are stripped (`strip = "symbols"`), but `dist` also writes line tables to a dSYM per binary (`debug = "line-tables-only"`, `split-debuginfo = "packed"`; rustc runs dsymutil before it strips). `scripts/collect-dsyms.sh` matches each binary to its dSYM under `target/` by UUID. The release workflow uses it and uploads the dSYMs as the run's `Fastab-v<version>-arm64-dSYMs` workflow artifact (90-day retention), never as a release asset: they are for us, not for users. A missing dSYM is a warning there, not a failed release; the Rust job's dist build on `main` runs the same script as a gate, so a profile change that stops producing dSYMs fails there first. `[profile.dist.package.libmimalloc-sys]` keeps `debug = false`: its build script defines `NDEBUG` and `MI_BUILD_RELEASE` only when Cargo hands it `DEBUG=false`, and the same CI step fails if `libmimalloc.a` carries debug sections. Symbolicate an `app+0x…` frame with `atos -arch arm64 -o fastab.dSYM/Contents/Resources/DWARF/fastab -l <load address> <address>`. Releases up to v0.0.19 have no dSYMs.

The CI `Binary size` job (main only, not a gate) builds the dist profile with symbols, runs `scripts/binary-size-report.sh` (cargo-bloat `--crates`, then `strip -x` and an LZMA size), and posts each binary's crate table as an annotation. Measure there before cutting a dependency. On the 2026-10-26 baseline, `fastab` was 20.2 MB stripped, `ftab` 14.4 MB, `fterm` 6.2 MB. That run is why these three are trimmed:

- GPUI's `image` keeps only `png`, `gif` and `webp` (GPUI names the GIF and WebP decoders directly), and `resvg` is built without `text`. Both are patches in `vendor/gpui` (see its `FASTAB_PATCHES.md`). Do not re-enable them for a format the app never decodes.
- The workspace `reqwest` has no `http2`. GPUI's own `gpui_http_client` still enables it on `zed-reqwest`, so `h2` stays in the desktop graph.
- `ftab engine complete` exists only with the `engine-cli` feature. CI lints and tests with `--features fastab_cli/engine-cli`; the shipped `ftab` is built without it.

A dependency change that alters the runtime graph also changes `THIRD_PARTY_NOTICES.txt`, which `build-app.sh` checks at release. Regenerate it with the Third-Party Notices workflow. When crates only leave the graph, deleting exactly their sections also works: walk `cargo metadata --offline --locked --filter-platform aarch64-apple-darwin` from the four runtime roots the way the generator does, and drop the `name version` blocks no longer reached. That is how the 2026-10-26 trim was done.

`prost_reflect` (~430 KB in each of `fastab`, `ftab` and `fterm`) is the next large item. It runs through the IPC framing (`fastab_ipc` receive paths, `ReflectMessage` in local IPC), so removing it is a refactor, not a feature flag.

## Key Crates

| Crate              | Role                                                             |
| ------------------ | ---------------------------------------------------------------- |
| `fastab_desktop`      | Native app host: GPUI overlay + settings, tray, engine client    |
| `fastab_gpui`          | Overlay list, theme, macOS window placement                      |
| `fastab_engine`        | Headless completion: IR lookup, generators, typed/adapter hooks  |
| `fastab_term`          | PTY interceptor, shell edit buffer tracking                      |
| `fastab_cli`           | CLI binary, all `ftab` subcommands                                 |
| `fastab_input_method` | macOS IMKit input method helper                                  |
| `fastab_hitoolbox`     | The HIToolbox palette lists, shared by the IME and the CLI        |
| `fastab_integrations` | Shell/terminal/editor integration install logic                  |
| `fastab_desktop_api`  | All that is left of the WebView bridge: the `install` request      |
| `fastab_ipc`          | Unix socket IPC primitives                                       |
| `fastab_proto`        | Generated Protobuf message types                                 |
| `fastab_settings`     | Settings persistence (JSON)                                      |
| `fastab_util`         | Shared constants, directory paths, system info                   |
| `macos-utils`      | macOS Accessibility API, NSWorkspace, AppKit ObjC2 bindings      |

## Key TypeScript Packages

| Package                 | Role                                                                 |
| ----------------------- | -------------------------------------------------------------------- |
| `autocomplete-parser`   | Used by `compile-spec-ir.mjs` to evaluate Fig specs at build time    |
| `shell-parser`          | Shell command-line tokenizer used by the spec compiler               |
| `api-bindings`          | Generated TS Protobuf IPC bindings                                   |
| `api-bindings-wrappers` | Ergonomic wrappers over `api-bindings`                               |

Everything under `packages/` now exists to serve `scripts/compile-spec-ir.mjs`; nothing there ships in the `.app`.

## Toolchain Versions

- Rust: `1.88.0` (pinned in `rust-toolchain.toml`), edition 2024
- Node: `>=22.13 <23`
- pnpm: `11.14.0` (see root `package.json` `packageManager`)
- Turborepo handles the TypeScript build graph

## macOS-Specific Notes

- The `.app` bundle lives at `/Applications/Fastab.app`
- Launch at login: `SMAppService.mainAppService` on macOS 13+; `~/Library/LaunchAgents/app.fastab.plist` fallback on macOS 12
- IME symlink target: `~/Library/Input Methods/FastabInputMethod.app`
- HIToolbox prefs (`com.apple.HIToolbox`) must include the IME bundle ID for Ghostty/Kitty cursor following to work
- `TISCreateInputSourceList` returns NULL when called without NSApplication; always call TIS APIs via `run_on_main` or from within the IME process itself
- Overlay Y conversion must use `NSScreen.screens[0]` (menu-bar / global-origin display). `NSScreen.mainScreen` is whichever display holds keyboard focus.
- Accessibility: never auto-open System Settings or the TCC sheet (not on launch, install, tray, or a granted→revoked reconcile). The settings page Grant button calls `begin_accessibility_guide()`, which opens the Accessibility pane (`Privacy_Accessibility` on every macOS version) and docks a draggable app-icon card beside it. macOS 27 labels that list **Device Control and Data Access** / **设备控制和数据访问**; macOS 26 and earlier still say Accessibility / 辅助功能. If this process is not trusted, the guide runs `tccutil reset Accessibility` for our bundle id first so a cdhash-stale list row is removed and the current binary can be dragged in; never reset without a parsed bundle id, and never on launch. The tray warning only opens Fastab Settings.
- Process memory: `./scripts/memory-usage.sh` (`--watch`, `--peak`, `--csv`). Reports `phys_footprint`, the same number as Activity Monitor. `fterm` appears as `<shell> (fterm)`. An `IOAccelerator` line on `ftab` or on an fterm from v0.0.20 or earlier is mimalloc's heap (VM tag 100), not GPU memory.
