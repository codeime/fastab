# Changelog

## v0.0.24

- change: the desktop app and each terminal tab should use much less memory. Both now start with macOS's space-efficient malloc setting, because by default freed memory stayed with the process. Started this way, v0.0.23 idled at 15 to 20 MB on the machine we tested, against 39 to 50 MB in earlier runs that also opened Settings and handled more completions. Each tab uses about 0.4 MB less. Completion times on CI did not change.
- change: each keystroke now builds at most 200 history rows, the newest matches. With a large shell history, `git ` matched more than 5,000 rows on every keystroke for a list that shows six. Older commands beyond those 200 appear once you type more of them.

## v0.0.23

- fix: a past command identical to what you typed no longer leads the list. With `git ch` in your shell history, typing `git ch` showed a `ch` row first, and accepting it inserted nothing.
- change: the history index takes less memory. Reading a full shell history in v0.0.22 made it the largest item the completion engine keeps, estimated at 2.5 MB for 10,000 commands.

## v0.0.22

- change: slow suggestions no longer hold up the list. A generator that took 80 ms or more last time now loads after the options and subcommands are shown.
- change: completions are faster after a pause. On CI, the first `git che` after one went from about 75 ms to about 25 ms and `cargo b` from about 128 ms to about 14 ms. While you keep typing in a token, `git che` went from about 19 ms to about 1 ms, because commands such as `git help -a` run once per command prefix instead of once per keystroke. Their results are reused for up to five minutes, so a new git alias or package.json script can take that long to appear.
- change: history suggestions for zsh and bash now come from your shell's history file, and fish reads its own file directly. Earlier versions tried to list zsh and bash history through a login shell, got nothing, and were left with only the commands Fastab had recorded. If you set `HISTFILE` in `.zshrc` without exporting it, Fastab reads the default path instead.
- change: history stays current while the app runs. Commands you finish in a Fastab terminal reach history suggestions and ranking within seconds, instead of at the next launch.
- change: opening a terminal starts `ftab` once per rc file instead of twice, and a prompt starts it only when your exports or aliases changed. On CI the integration added about 25 to 33 ms to a zsh start, down from about 38 ms, and about 14 ms with the app closed, down from about 29 ms.

## v0.0.21

- change: each terminal tab uses about a third less memory, roughly 1.2 MB less per tab. The terminal process now uses the system memory allocator instead of mimalloc, which held about 2 MB for a tab that needs well under 1 MB.
- fix: the desktop app is back on the system memory allocator. The switch to mimalloc in v0.0.20 did not give memory back as that entry said; measured on an installed build, it held several megabytes more after use.

## v0.0.20

- change: the desktop app should give memory back to the system after completions. Its own allocations now use mimalloc, so short-lived completion and drawing work no longer shares memory pages with what macOS frameworks keep for the life of the app; on v0.0.19 that left about 12 MB held but unused after a session. The app also runs three fewer idle threads.
- change: each terminal tab runs two fewer background threads and no longer wakes up every 30 seconds while idle.
- fix: uninstalling the input method with `ftab integrations uninstall input-method` or the uninstall script now removes its input source entries, stops the helper and removes its link, instead of stopping at the first step.
- fix: whether an update restarts the input method helper no longer depends on where the release was built. Updating to this version restarts it once.

## v0.0.19

- fix: updating FasTab no longer restarts the input method helper unless the helper itself changed, so Ghostty, Kitty, Otty and similar terminal windows keep cursor tracking across updates. Updating to this version still restarts it once.
- fix: the uninstall script now finishes every step and no longer closes open terminal tabs; tabs opened before uninstalling keep the old wrapper until they are closed.

## v0.0.18

- fix: quitting or restarting FasTab no longer stops the input method helper. macOS ended it a few seconds after the app quit, which left Ghostty, Kitty, Otty and similar terminals without cursor tracking until their windows were reopened. The app now also restarts the helper if it exits while enabled.

## v0.0.17

- fix: git completions run the spec's own commands again, as the WebView version and Fig did. `git checkout` and other branch arguments list the current branch first, show remote branches without their `origin/` prefix, and order the rest by most recent commit; commit, tag, remote, stash and changed-file suggestions get their original formatting back.
- change: the download is smaller: the disk image uses LZMA compression, and the desktop app and CLI drop image decoders, SVG text shaping and HTTP/2 code they never used (about 6 MB less on disk).
- change: the `ftab engine` debugging command is no longer included in the shipped CLI.

## v0.0.16

- change: return freed memory to the system once the app has been idle for 15 seconds, so the desktop app no longer stays near its peak memory use long after the last completion.

## v0.0.15

- change: pasting a large block of text into a running command no longer stalls the terminal; input parsing now grows linearly with the paste.
- change: complete faster for commands with many global options (such as kubectl and docker) and for generators that filter command output with regular expressions.
- change: do less work per keystroke in the terminal process and the completion overlay, use less memory for each terminal tab's key table, and drop an unused notification path from the desktop app.

## v0.0.14

- fix: replace settings.json atomically and stop rewriting it as `{}` when it fails to parse; an unparsable file is kept and backed up, and a symlinked settings file keeps its link.
- fix: count accepted-suggestion deletions and cursor moves in shell cursor positions, so emoji and accented names no longer delete the character before the token.
- fix: raise the open file limit at launch so large sessions with many terminal panes keep their completions.
- fix: update the Nushell integration to current Nushell syntax and quote shell init values safely.
- fix: keep a terminal tab open when the terminal reports a zero size, instead of crashing the session.
- change: reduce the memory each open terminal tab uses, by dropping unused system frameworks, allocating the alternate-screen buffer only while it is shown, and trimming allocator and database overhead.

## v0.0.13

- fix: recognize Otty 1.6 bashrc and bash-profile trailers so Settings reports a valid shell install as ready, and surface a repair error when the rewrite still fails the status check.

## v0.0.12

- fix: keep the completion list open when focus moves within the same VS Code / Cursor terminal pane, and dismiss it when another terminal tab takes focus.
- fix: drop input-method caret updates that belong to a previously focused window, and stop background tab prompt redraws from taking over the active session.
- fix: send a Tab back to the shell when a deferred completion has nothing to insert, and keep a stale search term from being inserted after the line has changed.
- fix: re-enable completion on the next keystroke only when the terminal that was disabled is no longer frontmost or its state could not be read.
- fix: settle invalid UTF-8 bytes and a lone Esc in the terminal input parser without stalling input, keep replayed keys behind earlier typing, and remove the session socket on exit.
- fix: raise the remote IPC connection cap and back off on accept errors instead of stopping or flooding the log.
- fix: keep requested font features and append system font fallbacks; defer system font enumeration until it is needed.
- fix: retire and restore native settings menus safely when the settings window closes and reopens.

## v0.0.11

- change: load native font faces only when selected or needed for fallback, reducing unused font retention while preserving font matching and stable font identities.
- test: cover native font selection, descriptor attributes and shaping, fallback precedence, and font identity reuse in remote CI.

## v0.0.10

- fix: prevent the host-only CoreVideo binding from entering generated Metal shaders, resolving the initial v0.0.10 build's crash when opening the first native window.
- test: check the actual generated shader header and embedded source in ordinary CI so this startup regression is caught without a GPU.
- fix: borrow the Metal device when creating CoreVideo texture caches, balancing native references across renderer lifetimes.
- change: create video texture caches only when a frame contains surfaces, avoiding unused caches in ordinary settings and completion windows.
- change: release native text-layout and child-layout scratch storage after the last window closes, preserving font identities and rebuilding buffers on demand.

## v0.0.9

- fix: release copied keyboard input sources and CoreText font attribute arrays after native queries, preventing retained references from accumulating.
- fix: keep font feature keys and values alive while constructing CoreText dictionaries.
- fix: remove closed-window entity tracking while preserving other windows and reusing stable drawing dependencies.
- change: release empty UI element arena storage after the last window closes, preserve live elements and allocate storage again only when needed.
- fix: clear completion insertion predictions when the current input ends or focus changes, while preserving delayed insertion echoes during ordinary hiding.

## v0.0.8

- change: use FasTab as the app's display name while preserving bundle identifiers, installation paths and command names for existing installations.
- change: bound asynchronous desktop and terminal event admission, connection lifetimes and incoming IPC frames; release oversized receive buffers and retired session text.
- fix: enforce synchronized terminal output size and time limits, allocate its buffer on demand and release oversized capacity after replay.
- change: reduce generator, history, environment and suggestion-rendering allocations; retire derived history indexes after the idle grace while preserving completion content and ranking.
- change: coalesce pending input-method caret updates into one slot and cancel superseded completion debounce tasks.
- fix: preserve complete PTY insertion writes, paste bytes and escape prefixes across read boundaries; prioritize ready key settlement before concurrent remote insertion and process ready PTY output before child exit.
- fix: reject delayed input from an earlier focus context, preserve fresh same-text input and prevent an old session's empty input from hiding a newer session's suggestions.

## v0.0.7

- change: defer hidden completion, caret queries, history materialization and hook catalog parsing until needed; release hidden suggestion payloads after the idle grace.
- change: request native file icons only for visible rows, cancel superseded directory scans, retain bounded candidates and reduce ranking allocations.
- fix: request fresh rows and caret geometry for cold Tab/Show, reject stale loading candidates and consume intercepted Tab exactly once when partial results become actionable or an insertion succeeds.
- fix: preserve temporary fuzzy matching during presentation-only settings refreshes; avoid settings-file reads on ordinary hidden keystrokes and coalesce identical queued input updates.
- fix: preserve terminal protocol ordering and invalid UTF-8 paste boundaries, drain parser controls locally, release large input buffers and settle intercepted keys on EOF or permanent errors.
- fix: bound IPC queues and supervised engine work, preserve idle-release deadlines, clean up script process groups before reaping and maintain RPC timeouts only while replies are pending.
- change: bound cross-process log rotation and native background workers; report build identity, resource ownership and reliable memory-measurement failures.

## v0.0.6

- fix: drain temporary AppKit objects after synchronous background queries and startup, preventing them from accumulating in thread autorelease pools.
- fix: replace and remove Accessibility observers by process identity, preserving other live instances of the same app.
- change: start the file-backed database pool with one idle connection and retire excess idle connections while preserving four concurrent checkouts.
- change: allocate Metal path textures only when a frame needs them, reusing matching sizes and retiring obsolete targets on resize.

## v0.0.5

- change: release rebuildable application-wide image and text caches after the last native window closes, preserving font identities and safely retiring buffers returned by older work.
- change: unload typed-hook descriptors and adapter bindings after the input's 10-second idle grace, reload them through the verified spec snapshot on demand, and expose catalog ownership in engine diagnostics.
- change: rename the PTY executable and new process titles to `fterm`; keep `fastabterm` compatibility entry points for existing shell integrations, with the Rust crate still named `fastab_term`.
- change: release unused parsed spec trees after a 10-second grace, including when the input is cleared, executed, or its terminal disconnects. Re-entering a command reloads its specs on demand with the same suggestion content and order.
- change: cooperatively cancel superseded completions and their scripts, reuse the engine, and prevent cancelled work from publishing incomplete cache or generator state.
- change: reduce temporary spec-loading allocations with streamed snapshot hashing and shared-option fingerprints; add numeric resource diagnostics and persistent replay scenarios for release and cancellation checks.
- fix: history indexing and pinned developer specs no longer keep unrelated bundled trees active. Release every cached tree for the same file, and preserve its pending deadline when LRU eviction removes only one of those trees.
- fix: preserve release notifications across cancelled session switches, allow expiry under a busy control queue, and remove unreachable trees after explicit replacement. Settings refreshes no longer resubmit input from an ended session.
- fix: invalidate cached history argument indexes when aliases or the shell change, and reject late indexes built for another context.
- fix: bound xterm Accessibility caret searches with one shared deadline and per-window failure backoff; reject stale focus/window results and clear unavailable caret positions.
- fix: retire completion resources when the popup is hidden or cannot be positioned, cancelling work and starting the 10-second release grace while allowing pending generators to finish before retiring an empty result.
- fix: queue terminal input before requesting caret updates, wait for a valid caret before submitting completion, and resume once when positioning recovers. Focus changes, window destruction and ended input discard retryable input so stale commands cannot reappear.
- fix: clean up remote IPC sessions and pending replies after writer failures, including when the read connection remains open.
- change: add numeric engine resource diagnostics and Otty Accessibility caret failure diagnostics. Verification covers source tests and review; a new installed build and real Otty completion behavior still need verification.
- fix: release idle Metal instance buffers when native windows close, and prevent late GPU completions from refilling the retired pool while other windows continue rendering.
- fix: avoid a reentrant window-focus deadlock when opening Settings alongside the completion popup.
- fix: release window renderers and Metal surfaces when Settings closes even if macOS retains its native view; discard late callbacks and stop closed-window drag loops without interrupting other windows or in-flight GPU work.
- fix: bound decoded file icons, retire unused icons after 10 seconds hidden, and remove stale Metal atlas entries safely without invalidating in-flight GPU reads.
- fix: reuse a display refresh clock across windows and release retired frame subscriptions instead of accumulating native objects on each window cycle.
- fix: balance native text and window-string ownership, handle optional text-query outputs, and prevent closed permission-guide tasks from joining a reopened guide.
- fix: release specs first loaded by history-only completion after the idle grace, preserving existing input ownership and release deadlines.
- change: Tab extends a shared prefix when possible, then accepts the selected non-action suggestion when no further prefix can be inserted; multi-row execution actions retain their Tab guard, and custom bindings still take precedence.
- fix: recognize CSI-u Tab, Enter, Escape and Backspace sequences as functional keys so configured completion actions can intercept them.
- change: release the completion window and its renderer after 10 seconds hidden, retaining suggestions for on-demand restoration; hidden caret updates no longer recreate a window just because an argument hint remains.
- change: limit each asynchronous file/stdout log queue to 4,096 messages instead of 128,000, reducing permanent empty-queue storage while retaining non-blocking, lossy writes and shutdown flushing.
- fix: repeated empty-input or hide notifications no longer postpone the hidden completion window's 10-second release deadline; showing the window or processing new results still cancels the old deadline.
- fix: a delayed Show or multi-row Tab action without a caret keeps the overlay hidden and its retirement deadline intact; an available caret resumes completion once.
- change: cap each hook cache at 4 MiB of estimated payload and release idle hook results after 10 seconds, including caches left by cancelled requests; release empty suggestion buffers with the hidden window.
- fix: bound legacy acceptance preferences by count, size and key length, pruning old data while preserving newer timestamps, command isolation and the existing JSON format.
- fix: limit concurrent AI repository probes through the lifetime of the actual filesystem work, so cancelled or timed-out requests cannot accumulate blocking jobs.
- change: temporarily hide AI Settings and route its old links to Appearance without changing saved AI configuration or consent.

## v0.0.4

- change: a command group that points at another spec file loads when the cursor enters it. The gcloud menu keeps each group's stub name and description; entering `gcloud compute ` parses `compute.json`, and groups that were not entered stay on disk. `git` completions stay the same.
- change: suggestion lists and subcommand entry share the cached spec instead of copying the tree. Names, order, hidden rows, and dependency priority 75 stay the same. Accepting a group from the first menu inserts that stub, so a trailing space and a required-argument hint appear after the group is entered.

## v0.0.3

- change: keep AI Settings focused on enablement, provider configuration and connection testing; removing the session diagnostics card and Git status control does not itself change saved consent or Git-sharing values. Anonymous, bounded AI aggregates remain available through the existing diagnostics command at DEBUG level.
- fix: use the actual Accessibility insertion point in Otty 1.2+, retain IME support for older or unknown versions, and discard stale focus/caret updates. Clear old coordinates and key interception when the caret is unavailable, and keep IME state reads off the UI thread; installed-app verification is pending.

- change: simplify the gold AI icon to one primary sparkle and one accent, and use the same shape for recommendations and loading/status indicators
- fix: allocate the AI candidate budget after local ranking while retaining provenance checks before deduplication
- feat: learn argument preferences within the current directory and parser argument slot, independently of global command preferences
- fix: validate bounded local Git and history context before reusing recommendations while preserving existing Git-sharing consent
- feat: cache validated recommendation IDs in memory for 15 seconds, with a 64-entry limit and context, settings and navigation invalidation
- fix: reply to repeated input-method caret requests even when the cursor has not moved, restoring caret state after session changes or desktop restarts while ignoring background terminal windows
- fix: allow horizontal trackpad scrolling through long completion names and argument hints; manual scrolling pauses the row's marquee and rejects late AI recommendations without changing the current selection or order
- fix: Up from the first suggestion wraps to the last by default, while respecting explicit wrap and shell-history navigation preferences
- change: organize AI settings into enable, basic and always-expanded advanced cards; automatically save completed edits, including when leaving or closing settings
- fix: keep API key drafts bound to their service address and finish queued connection tests with clear feedback when settings cannot be saved
- fix: persist Fastab input-method palette registration on startup without automatically opening macOS Keyboard settings
- feat: include current input, Git branch and bounded recent command history in Jev recommendations, with updated data-use notices
- fix: preserve SQLite's cross-process locks while restricting database permissions, preventing intermittent SIGBUS crashes during shell initialization
- change: store Jev API keys as plaintext in local SQLite with user-only file permissions and masked saved values; do not read or migrate old Keychain entries
- feat: enable AI recommendations for more verified Git, Cargo and Homebrew subcommands and options
- change: Ctrl-K shows full argument details while the normal footer stays on one line
- fix: silently retain local suggestions when AI chooses not to reorder them, times out or encounters a temporary failure
- fix: native Edit menu actions now use GPUI's action table, preventing the Select All deadlock and preserving keyboard editing shortcuts
- fix: restore a previously chosen input method on launch; serialize install and removal without blocking async workers or changing installation state during status checks
- change: AI settings use colored connection results, the standard settings switch and clear provider selection; a short data-use notice replaces the separate consent toggle
- fix: saved API keys show a masked placeholder without loading the secret into the input; background presence checks discard stale results and wait for pending credential writes
- fix: AI credential readiness no longer restarts local completion or replaces existing suggestions with the engine loading indicator
- feat: show a gently fading AI icon at the bottom left of the completion popup during requests, while local suggestions remain usable
- fix: Jev settings pause requests immediately and save in the background, without blocking input on file locks; stale saves cannot replace newer settings
- fix: failed AI saves remain retryable while AI stays paused; newer edits always take precedence over pending saves
- fix: ignore delayed terminal focus events after switching apps and bound Accessibility queries; settings credential waits now time out without losing the key draft, while runtime reads retain late results
- fix: Tab navigation now reaches AI inputs, actions and theme selectors, skipping disabled controls
- change: simplify AI setup and move model, endpoint details and saved profiles into advanced settings
- feat: test the current Jev key, model and endpoint with a fixed example and clear connection-error feedback
- fix: remote IPC reconnect no longer stops permanently when an in-flight outbox frame still holds budget accounting (`Busy` vs `Stopped`)
- fix: intercepted-key replay no longer blocks the PTY main loop for up to 5s; ordinary input and desktop Inserts stay ordered behind pending keys without a barrier wait
- fix: generator subprocess cleanup stays bounded after kill; unfinished children are reaped in the background instead of hanging `wait` or leaving zombies
- fix: enabling Jev AI recommendations fails closed with a clear settings status when the bundled specs-ir public baseline pins do not match
- fix: CI `cargo fmt --check` passes across the workspace
- change: terminal grid row-cache reclaim uses hysteresis so short-lived resizes do not thrash capacity
- feat: opt-in Jev completion recommendations (settings + provider profiles), promoting one existing candidate when local results are ready
- fix: Jev API key entry handles ASCII keys directly without macOS text composition; pasted keys accept surrounding whitespace, and blank or invalid pastes preserve the previous value
- fix: unchanged settings input no longer pauses Jev, and failed credential or configuration saves retain the key draft for retry
- change: Jev settings are their own Settings page

## v0.0.2

- fix: completion kinds match the previous parser. Once a positional argument is consumed, or after `--`, subcommands leave the list, so fuzzy search no longer offers unrelated commands such as `merge` for `git check m`
- fix: the app icon is the previous dark tile again, with the green prompt and completion list painted into `icon.icns`. The ochre gradient and the layered icon catalog are gone, so macOS 26 no longer shows a plain background
- fix: granting Accessibility no longer quits the app. The drag row's name label was released while the card was still showing it
- fix: the Accessibility grant card centers the icon with its name and the title with the close button, and animates an arrow toward the list
- fix: display-language chips use a static option id so the settings window compiles
- change: the DMG no longer stamps a custom volume icon
- change: shell history suggestions use the same recent window as the on-disk history database
- change: the first-run permission page can switch display language before the rest of settings is available
- change: that language control sits below the window title bar so the click reaches the page
- docs: how to uninstall Easy Complete before installing Fastab

## v0.0.1

First Fastab release. Versioning starts here; earlier Easy Complete tags are retired.

Fastab is a macOS terminal autocomplete app. Completions follow the caret in a native overlay.

- Overlay and settings are native GPUI views. Completions do not enter a WebView, and the app ships no JavaScript runtime: specs compile to typed IR or named Rust adapters.
- Product identity is Fastab only: `Fastab.app`, bundle `app.fastab`, CLI `ftab`, PTY `fastabterm`, URL scheme `fastab://`.
- Fastab can sit beside Easy Complete, Fig, or Amazon Q. Install, uninstall, IME, HIToolbox, and data dirs are Fastab-only. The same terminal session cannot run two completion desktops; `ftab init` stands down when another PTY already wraps the shell.
- Telemetry UI is hidden and reporting stays unconfigured. Completions stay on-device.
- The Apple Silicon DMG is ad-hoc unsigned. After copying to `/Applications`, clear Gatekeeper quarantine with `xattr -dr com.apple.quarantine "/Applications/Fastab.app"`. Accessibility is Fastab's own TCC identity (`app.fastab`); grant it from Fastab Settings.
