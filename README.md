<p align="center">
  <img src="./assets/logo.png" alt="FasTab" width="140px">
</p>

<h1 align="center">FasTab (Native)</h1>

<p align="center">
  <b>IDE-style inline autocomplete for your macOS terminal — native GPUI, not a WebView.</b><br/>
  An open-source, Fig-style completion engine for <code>zsh</code>, <code>bash</code> & <code>fish</code>.
</p>

<p align="center">
  <a href="https://github.com/codeime/easy-complete/releases"><img alt="Release" src="https://img.shields.io/github/v/release/codeime/easy-complete?color=brightgreen"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS-lightgrey">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange">
  <img alt="Native GPUI" src="https://img.shields.io/badge/UI-native%20GPUI-8A2BE2">
  <a href="#-license"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>
  <a href="https://github.com/codeime/easy-complete/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/codeime/easy-complete?style=social"></a>
</p>

<p align="center">
  <b>English</b> · <a href="./README.zh-CN.md">简体中文</a>
</p>

**FasTab (Native)** is a macOS terminal autocomplete app — IDE-style inline
completions for your shell, rendered in a native GPUI overlay that follows your
cursor. The popup and the settings window are real native views, not WKWebView.
Completions run in a local Rust engine. It is a local-first completion engine
focused purely on autocomplete — a lightweight, fully local alternative to Fig.

You get fish-shell-style suggestions for hundreds of CLIs (`git`, `npm`, `docker`,
`cargo`, …): flags, subcommands, file paths, and arguments, completed as you type.
Autocomplete runs fully on-device — no account, no cloud calls, no AI requests, and
your commands never leave your Mac. **This fork has all telemetry off and collects
nothing.** See the [Privacy page](https://fastab.app/privacy-policy).

<p align="center">
  <img src="./.github/media/screenshot.png" alt="FasTab autocomplete in action">
</p>

> **Platform:** macOS only. The published DMG is Apple Silicon / ARM64 only.

## Native

The completion popup and the settings window are GPUI views (Zed's UI toolkit).
Completions never enter a web view, and the app ships no JavaScript runtime:
`fastab_engine` looks up JSON IR compiled at build time, and every spec hook
(`postProcess`, `script`, `custom`, `generateSpec`) is compiled to typed IR or
a named Rust adapter at build time too.

## Contents

- [Native](#native)
- [Install](#-install)
- [Coming from Easy Complete](#coming-from-easy-complete)
- [Usage](#-usage)
- [Uninstall](#-uninstall)
- [How it works](#-how-it-works)
- [Development](#-development)
- [License](#-license)

---

## ⚡️ Install

### Download the DMG

Native builds are the Apple Silicon DMGs from this repository:

[Download latest DMG](https://github.com/codeime/easy-complete/releases/download/v0.0.26/Fastab-arm64.dmg) ·
[All releases](https://github.com/codeime/easy-complete/releases)

Then:

1. Open `Fastab-arm64.dmg`.
2. Drag **Fastab.app** into `/Applications`.
3. **Current builds are not Developer ID signed.** After copying the app, clear Gatekeeper quarantine once, or macOS will block launch:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Fastab.app"
   ```

4. Launch **FasTab** from `/Applications`.
5. Open FasTab Settings and click **Grant Accessibility**.
6. Reload your shell:

   ```bash
   exec $SHELL
   ```

On first launch, FasTab sets up the bundled CLI binaries, shell integration,
and login startup entries. The input method is optional — install it from Settings →
Behavior, or with `ftab integrations install input-method`, for Ghostty, Kitty,
WezTerm, Zed, Alacritty, and Otty. To verify the installation, run:

```bash
ftab doctor
```

### Build from source

For development, or if you need to build locally, clone the repository and run the
installer:

```bash
git clone https://github.com/codeime/easy-complete.git
cd easy-complete
./scripts/install.sh
```

The source installer will:

1. Build the Rust binaries and compile bundled completion specs.
2. Assemble `Fastab.app` and copy it to `/Applications`.
3. Symlink the `ftab` and `fterm` CLIs into `~/.local/bin`, keeping `fastabterm` as a compatibility alias for existing shell integrations.
4. Let you enable **Launch at Login** from Settings (a system Login Item on macOS 13+, with a LaunchAgent fallback on macOS 12).
5. Set up shell integration. `./scripts/install.sh` also registers the optional input method (DMG first launch does not).
6. Leave Accessibility for you to grant from FasTab Settings (required — see below).

When it finishes, reload your shell:

```bash
exec $SHELL
```

### Grant Accessibility permission

FasTab positions the completion popup relative to your focused terminal
window, which requires the macOS **Accessibility** permission. Open FasTab
Settings and click **Grant Accessibility**. That opens:

> System Settings → Privacy & Security → Device Control and Data Access
>
> On macOS 26 and earlier the same list is named **Accessibility**. Grant still
> opens it.

and floats a card you can drag **FasTab** from into the list. The app never
opens that pane on its own.

FasTab is a **separate product** from Easy Complete. Both can be installed
at the same time (`app.fastab` vs `dev.emmmm.easy-complete`). Grant
Accessibility to FasTab itself — Easy Complete's checkbox does not cover
this binary. Do not run both completion desktops in the same terminal
session (that would nest PTYs).

If completions never appear, this is almost always the cause. Run the same flow
again from Settings, or with:

```bash
ftab debug prompt-accessibility
```

### Coming from Easy Complete

FasTab will not uninstall Easy Complete. To switch cleanly, follow
[Uninstall Easy Complete](./docs/uninstall-easy-complete.md) first, then
install FasTab and open a **new** terminal. An old `ecterm` tab still has
`Q_TERM` set, so `ftab init` stands down.

---

## 🚀 Usage

Once installed and granted permission, just start typing in any supported terminal —
suggestions appear inline as you type.

| Key             | Action                            |
| --------------- | --------------------------------- |
| `↑` / `↓`       | Move through suggestions          |
| `⇥` (Tab) / `→` | Accept the highlighted suggestion |
| `Esc`           | Dismiss the popup                 |

The native settings window is available from the **FasTab menu bar icon**
(system tray).

Useful CLI commands:

```bash
ftab doctor                       # diagnose common problems
ftab diagnostic                   # print environment / integration status
ftab integrations install input-method   # (re)register the macOS input method
ftab settings list                # view settings
ftab settings <key> <value>       # change a setting
```

### Supported terminals

Most terminals work out of the box via the PTY integration — including iTerm2, Apple
Terminal, VS Code, Cursor, ChatGPT (Codex), and JetBrains IDE terminals. Terminals that
bypass the standard PTY path (**Ghostty, Kitty, WezTerm, Zed, Alacritty, Otty**)
additionally rely on the bundled input method for cursor tracking. Install it from
Settings → Behavior, or with `ftab integrations install input-method`.

---

## 🗑️ Uninstall

```bash
./scripts/uninstall.sh
```

This removes the app bundle, CLI symlinks, LaunchAgent, input method, shell
integration, and all application data. It surgically removes only FasTab's own
input source from the system preferences (your other keyboard layouts and input
methods are left untouched). Easy Complete, Fig, and Amazon Q stay installed.

To remove Easy Complete, see [Uninstall Easy Complete](./docs/uninstall-easy-complete.md).

---

## 🧩 How it works

FasTab runs as three cooperating native processes that talk over Unix domain
sockets (Protobuf messages):

| Binary          | Crate         | Role                                                                                                                             |
| --------------- | ------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `fastab` | `fastab_desktop` | Native app host — GPUI overlay and settings (not WKWebView), completion engine worker, system tray, and window management |
| `fterm`        | `fastab_term`     | Pseudoterminal between your shell and terminal emulator; intercepts the shell edit buffer to drive completions                   |
| `ftab`            | `fastab_cli`      | CLI entry point — `setup`, `integrations`, `diagnostic`, `settings`, and more                                                    |

Shell hooks (`.zshrc`, `.bashrc`, fish config) report shell state — CWD, command text,
cursor position — back to `fterm` on every prompt and keystroke. On macOS, the
`fastab_input_method` helper app reports caret position for terminals that bypass the PTY.

**Identifiers**

- App bundle ID: `app.fastab`
- Input method bundle ID: `app.fastab.inputmethod`
- App bundle: `/Applications/Fastab.app`

---

## 🛠️ Development

### Toolchain

- Rust `1.88.0` (pinned in `rust-toolchain.toml`), edition 2024
- Node `>=22.13 <23`, pnpm `11.14`
- Turborepo for the TypeScript build graph

### Rust

```bash
# Build all release binaries
cargo build --release -p fastab_desktop -p fastab_term -p fastab_cli -p fastab_input_method

# Run a single crate in dev mode
cargo run --bin ftab -- <subcommand>
cargo run --bin fastab

cargo clippy --locked --workspace --color always -- -D warnings   # lint (CI: -D warnings)
cargo fmt                                                         # format
cargo test -p <crate_name>                                        # test a crate
```

### TypeScript

```bash
pnpm turbo build --filter="./packages/*"   # build all packages
node scripts/compile-spec-ir.mjs            # Fig specs → JSON IR + JS hooks
pnpm lint                                   # lint
pnpm test                                   # run Vitest
```

Headless completion (no overlay): `cargo run --bin ftab -- engine complete --buffer "git ch"`.
Process memory: `./scripts/memory-usage.sh` (`--watch 5`, `--peak`, `--csv mem.csv`).

### Key crates

| Crate                   | Role                                                             |
| ----------------------- | ---------------------------------------------------------------- |
| `fastab_desktop`           | Native app host: GPUI overlay + settings, tray, engine client    |
| `fastab_gpui`               | Overlay list, theme, macOS window placement                      |
| `fastab_engine`             | Headless completion: IR lookup, generators, typed hook IR        |
| `fastab_term`               | PTY interceptor, shell edit-buffer tracking                      |
| `fastab_cli`                | CLI crate, providing the `ftab` binary and all its subcommands     |
| `fastab_input_method`      | macOS input method helper (cursor tracking)                      |
| `fastab_integrations`      | Shell/terminal/editor integration install logic                  |
| `fastab_ipc` / `fastab_proto` | Unix-socket IPC primitives & generated Protobuf types            |

### Key TypeScript packages

| Package               | Role                                                              |
| --------------------- | ----------------------------------------------------------------- |
| `autocomplete-parser` | Evaluates Fig specs at build time for `compile-spec-ir.mjs`       |
| `shell-parser`        | Shell command-line tokenizer                                      |
| `api-bindings`        | Generated TS Protobuf IPC bindings                                |

---

## 📜 License

Licensed under the MIT License. FasTab is a separate product forked from
Easy Complete, based on the Amazon Q Developer CLI and Fig; those copyright
notices stay in
[LICENSE](./LICENSE). Third-party terms are collected in
[THIRD_PARTY_NOTICES.txt](./THIRD_PARTY_NOTICES.txt).
