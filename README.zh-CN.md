<p align="center">
  <img src="./assets/logo.png" alt="FasTab" width="140px">
</p>

<h1 align="center">FasTab (Native)</h1>

<p align="center">
  <b>为 macOS 终端打造的 IDE 风格行内自动补全——原生 GPUI，不是 WebView。</b><br/>
  一款开源、纯本地、Fig 风格的命令行补全引擎，支持 <code>zsh</code>、<code>bash</code> 与 <code>fish</code>。
</p>

<p align="center">
  <a href="https://github.com/codeime/easy-complete/releases"><img alt="Release" src="https://img.shields.io/github/v/release/codeime/easy-complete?color=brightgreen"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS-lightgrey">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange">
  <img alt="Native GPUI" src="https://img.shields.io/badge/UI-native%20GPUI-8A2BE2">
  <a href="#-许可证"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>
  <a href="https://github.com/codeime/easy-complete/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/codeime/easy-complete?style=social"></a>
</p>

<p align="center">
  <a href="./README.md">English</a> · <b>简体中文</b>
</p>

**FasTab (Native)** 是一款 macOS 终端自动补全应用——以原生 GPUI 浮层跟随光标，
为你的 shell 提供 IDE 风格的行内补全。补全列表和设置窗口都是真正的原生视图，不是
WKWebView。补全引擎在本地 Rust 里运行。它只专注于终端自动补全这一件事——是一款轻量、
完全本地、开源的 Fig 替代品。

你会在输入 `git`、`npm`、`docker`、`cargo` 等数百种命令行工具时，获得类似 fish shell 的
建议：参数、子命令、文件路径、选项，边打边补。
自动补全完全在本机运行——无需账号、无云端调用、无 AI 请求，你的命令内容永远不会离开你的
Mac。**本 fork 已关闭全部遥测，不采集任何数据。** 详见[隐私页面](https://fastab.app/privacy-policy)。

<p align="center">
  <img src="./.github/media/screenshot.png" alt="FasTab 自动补全效果">
</p>

> **平台：** 仅支持 macOS。当前发布的 DMG 仅支持 Apple Silicon / ARM64。

## Native

补全浮层和设置窗口是 GPUI 视图（Zed 的 UI 工具包）。补全本身从不进 WebView，
应用里也没有 JavaScript 运行时：`fastab_engine` 查的是构建期编好的 JSON IR，
spec hook（`postProcess`、`script`、`custom`、`generateSpec`）同样在构建期
编译成 typed IR 或具名 Rust 适配器。

## 目录

- [Native](#native)
- [安装](#-安装)
- [从 Easy Complete 迁过来](#从-easy-complete-迁过来)
- [使用](#-使用)
- [卸载](#-卸载)
- [工作原理](#-工作原理)
- [开发](#-开发)
- [许可证](#-许可证)

---

## ⚡️ 安装

### 下载 DMG

Native 构建是本仓库的 Apple Silicon DMG：

[下载最新版 DMG](https://github.com/codeime/easy-complete/releases/download/v0.0.24/Fastab-arm64.dmg) ·
[所有 Releases](https://github.com/codeime/easy-complete/releases)

然后：

1. 打开 `Fastab-arm64.dmg`。
2. 把 **Fastab.app** 拖到 `/Applications`。
3. **当前构建未做 Developer ID 签名。** 拷贝完成后先清一次隔离属性，否则 macOS 会拦截启动：

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Fastab.app"
   ```

4. 从 `/Applications` 启动 **FasTab**。
5. 打开 FasTab 设置，点击**授予辅助功能权限**。
6. 重新加载你的 shell：

   ```bash
   exec $SHELL
   ```

首次启动时，FasTab 会设置随附的 CLI 二进制、shell 集成和登录启动项。输入法是可选项，
可在设置 → 行为里安装，或运行 `ftab integrations install input-method`，供 Ghostty、Kitty、
WezTerm、Zed、Alacritty 和 Otty 使用。可以运行下面的命令确认安装状态：

```bash
ftab doctor
```

### 从源码构建

如果你要做开发，或需要在本机自行构建，可以克隆仓库并运行安装脚本：

```bash
git clone https://github.com/codeime/easy-complete.git
cd easy-complete
./scripts/install.sh
```

源码安装脚本会：

1. 构建 Rust 二进制，并编译打包的补全 spec。
2. 组装出 `Fastab.app` 并复制到 `/Applications`。
3. 把 `ftab` 和 `fterm` 两个 CLI 软链到 `~/.local/bin`，保留 `fastabterm` 兼容别名供已有 shell 集成使用。
4. 可在设置中开启**登录时启动**（macOS 13+ 使用系统登录项，macOS 12 回退到 LaunchAgent）。
5. 配置 shell 集成。`./scripts/install.sh` 还会注册可选输入法（DMG 首次启动不会）。
6. **辅助功能**需要你在 FasTab 设置里手动授予（必需，见下文）。

完成后，重新加载你的 shell：

```bash
exec $SHELL
```

### 授予「辅助功能」权限

FasTab 需要把补全浮层定位到你当前聚焦的终端窗口，这依赖 macOS 的**辅助功能
（Accessibility）**权限。打开 FasTab 设置，点击**授予辅助功能权限**。应用会打开：

> 系统设置 → 隐私与安全 → 设备控制和数据访问
>
> macOS 26 及更早，同一列表名为**辅助功能**。授予按钮仍会打开它。

并把一张可拖拽的卡片停在列表旁边，把 **FasTab** 拖进去即可。启动、安装和菜单栏
都不会自动打开系统设置。

FasTab 和 Easy Complete 是**两个产品**，可以同时安装（`app.fastab` 与
`dev.emmmm.easy-complete`）。请单独给 FasTab 授辅助功能——Easy Complete
的勾选不覆盖这个二进制。不要在同一个终端会话里同时跑两套补全（会套 PTY）。

**如果补全始终不出现，几乎都是这个权限没授予。** 在设置里再点一次按钮，或运行：

```bash
ftab debug prompt-accessibility
```

### 从 Easy Complete 迁过来

FasTab **不会**卸 Easy Complete。要干净切换，先按
[卸载 Easy Complete](./docs/uninstall-easy-complete.zh-CN.md) 做完，再装
FasTab，并**新开一个终端**。旧的 `ecterm` 标签里还带着 `Q_TERM`，`ftab init`
会让路。

---

## 🚀 使用

安装并授权后，在任意受支持的终端里直接开始输入即可——建议会随输入实时出现在行内。

| 按键            | 操作           |
| --------------- | -------------- |
| `↑` / `↓`       | 在建议间移动   |
| `⇥` (Tab) / `→` | 采用高亮的建议 |
| `Esc`           | 关闭补全浮层   |

原生设置窗口可从**菜单栏的 FasTab 图标**打开。

常用 CLI 命令：

```bash
ftab doctor                       # 诊断常见问题
ftab diagnostic                   # 打印环境 / 集成状态
ftab integrations install input-method   # （重新）注册 macOS 输入法
ftab settings list                # 查看设置
ftab settings <key> <value>       # 修改某项设置
```

### 受支持的终端

大多数终端通过 PTY 集成开箱即用——包括 iTerm2、Apple Terminal、VS Code、Cursor、
ChatGPT（Codex）以及 JetBrains IDE 终端。少数绕过标准 PTY 路径的终端（**Ghostty、Kitty、
WezTerm、Zed、Alacritty、Otty**）还需要依赖随附的输入法来追踪光标位置。可在设置 → 行为里
安装，或运行 `ftab integrations install input-method`。

---

## 🗑️ 卸载

```bash
./scripts/uninstall.sh
```

该脚本会移除应用包、CLI 软链、LaunchAgent、输入法、shell 集成以及全部应用数据。它只会
精确移除 FasTab 自己的输入源，**不会动**你其它的键盘布局和输入法。Easy Complete、
Fig、Amazon Q 会留着。

要卸 Easy Complete，见 [卸载 Easy Complete](./docs/uninstall-easy-complete.zh-CN.md)。

---

## 🧩 工作原理

FasTab 由三个相互协作的原生进程组成，通过 Unix 域套接字（Protobuf 消息）通信：

| 二进制          | Crate         | 职责                                                                                           |
| --------------- | ------------- | ---------------------------------------------------------------------------------------------- |
| `fastab` | `fastab_desktop` | 原生应用宿主——GPUI 补全浮层与设置窗口（不是 WKWebView）、补全引擎工作线程、系统托盘、窗口管理 |
| `fterm`        | `fastab_term`     | 介于 shell 与终端模拟器之间的伪终端；拦截 shell 编辑缓冲区以驱动补全                           |
| `ftab`            | `fastab_cli`      | CLI 入口——`setup`、`integrations`、`diagnostic`、`settings` 等                                 |

Shell 钩子（`.zshrc`、`.bashrc`、fish 配置）在每次提示符和按键时，把 shell 状态（当前目
录、命令文本、光标位置）回报给 `fterm`。在 macOS 上，`fastab_input_method` 辅助应用负责为绕
过 PTY 的终端上报光标位置。

**标识符**

- 应用 bundle ID：`app.fastab`
- 输入法 bundle ID：`app.fastab.inputmethod`
- 应用包路径：`/Applications/Fastab.app`

---

## 🛠️ 开发

### 工具链

- Rust `1.88.0`（在 `rust-toolchain.toml` 中固定），edition 2024
- Node `>=22.13 <23`，pnpm `11.14`
- TypeScript 构建图由 Turborepo 管理

### Rust

```bash
# 构建所有 release 二进制
cargo build --release -p fastab_desktop -p fastab_term -p fastab_cli -p fastab_input_method

# 以 dev 模式运行单个 crate
cargo run --bin ftab -- <子命令>
cargo run --bin fastab

cargo clippy --locked --workspace --color always -- -D warnings   # lint（CI 要求 -D warnings）
cargo fmt                                                         # 格式化
cargo test -p <crate_name>                                        # 测试某个 crate
```

### TypeScript

```bash
pnpm turbo build --filter="./packages/*"   # 构建所有包
node scripts/compile-spec-ir.mjs            # Fig spec → JSON IR + JS hook
pnpm lint                                   # lint
pnpm test                                   # 运行 Vitest
```

无界面补全：`cargo run --bin ftab -- engine complete --buffer "git ch"`。
进程内存：`./scripts/memory-usage.sh`（`--watch 5`、`--peak`、`--csv mem.csv`）。

### 核心 crate

| Crate                   | 职责                                                    |
| ----------------------- | ------------------------------------------------------- |
| `fastab_desktop`           | 原生应用宿主：GPUI 浮层与设置、托盘、引擎客户端         |
| `fastab_gpui`               | 补全列表、主题、macOS 窗口定位                          |
| `fastab_engine`             | 无界面补全：IR 查找、生成器、typed hook IR              |
| `fastab_term`               | PTY 拦截、shell 编辑缓冲区追踪                          |
| `fastab_cli`                | CLI crate，提供 `ftab` 二进制及其所有子命令               |
| `fastab_input_method`      | macOS 输入法辅助应用（光标追踪）                        |
| `fastab_integrations`      | shell / 终端 / 编辑器集成的安装逻辑                     |
| `fastab_ipc` / `fastab_proto` | Unix 套接字 IPC 原语与生成的 Protobuf 类型              |

### 核心 TypeScript 包

| 包                    | 职责                                          |
| --------------------- | --------------------------------------------- |
| `autocomplete-parser` | 构建时由 `compile-spec-ir.mjs` 用来执行 Fig spec |
| `shell-parser`        | shell 命令行分词器                            |
| `api-bindings`        | 生成的 TS Protobuf IPC 绑定                   |

---

## 📜 许可证

采用 MIT 许可证。FasTab 是独立产品，fork 自 Easy Complete，基于 Amazon Q
Developer CLI 与 Fig；这些版权声明保留在 [LICENSE](./LICENSE)。
第三方版权与许可证条款集中收录于
[THIRD_PARTY_NOTICES.txt](./THIRD_PARTY_NOTICES.txt)。
