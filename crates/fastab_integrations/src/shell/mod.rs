use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use async_trait::async_trait;
use cfg_if::cfg_if;
use clap::ValueEnum;
use fastab_os_shim::Env;
use fastab_util::{CLI_BINARY_NAME, INTEGRATION_MARKER_NAME, PTY_BINARY_NAME, Shell, directories};
use regex::{Regex, RegexSet};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorExt, Result};
use crate::{Error, FileIntegration, Integration, backup_file};

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum When {
    Pre,
    Post,
}

impl When {
    pub fn all() -> [When; 2] {
        [Self::Pre, Self::Post]
    }
}

impl std::fmt::Display for When {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            When::Pre => write!(f, "pre"),
            When::Post => write!(f, "post"),
        }
    }
}

fn quoted_pty_binary_path(shell: Shell) -> String {
    // CLI init snapshots must not capture a test runner's machine-specific path.
    let path = if Env::new().q_init_snapshot_test() {
        PathBuf::from("/Applications/Fastab.app/Contents/MacOS").join(PTY_BINARY_NAME)
    } else {
        fastab_util::current_pty_binary_path().unwrap_or_default()
    };
    let path = path.to_string_lossy();
    match shell {
        Shell::Bash | Shell::Zsh => format!("'{}'", path.replace('\'', "'\\''")),
        Shell::Fish => format!("'{}'", path.replace('\\', "\\\\").replace('\'', "\\'")),
        Shell::Nu => serde_json::to_string(path.as_ref()).unwrap_or_else(|_| "\"\"".into()),
    }
}

fn integration_file_name(dotfile_name: &str, when: &When, shell: &Shell) -> String {
    format!(
        "{}.{when}.{shell}",
        Regex::new(r"^\.").unwrap().replace_all(dotfile_name, ""),
    )
}

pub trait ShellExt {
    fn get_shell_integrations(&self, env: &Env) -> Result<Vec<Box<dyn ShellIntegration>>>;
    /// Script integrations are installed into ~/.fig/shell
    fn get_script_integrations(&self) -> Result<Vec<ShellScriptShellIntegration>>;
    fn get_fig_integration_source(&self, when: &When) -> String;
}

impl ShellExt for Shell {
    fn get_script_integrations(&self) -> Result<Vec<ShellScriptShellIntegration>> {
        let mut integrations = vec![];

        for file in match self {
            Shell::Bash => [".bashrc", ".bash_profile", ".bash_login", ".profile"].iter(),
            Shell::Zsh => [".zshrc", ".zprofile"].iter(),
            Shell::Fish | Shell::Nu => [].iter(),
        } {
            for when in &When::all() {
                let path = directories::fig_data_dir()?
                    .join("shell")
                    .join(integration_file_name(file, when, self));

                integrations.push(ShellScriptShellIntegration {
                    shell: *self,
                    when: *when,
                    path,
                });
            }
        }

        Ok(integrations)
    }

    fn get_shell_integrations(&self, env: &Env) -> Result<Vec<Box<dyn ShellIntegration>>> {
        let config_dir = self.get_config_directory(env)?;

        let integrations: Vec<Box<dyn ShellIntegration>> = match self {
            Shell::Bash => {
                let mut configs = vec![".bashrc"];
                let other_configs = [".profile", ".bash_login", ".bash_profile"];

                configs.extend(other_configs.into_iter().filter(|f| config_dir.join(f).exists()));

                // Include .profile if none of [.profile, .bash_login, .bash_profile] exist.
                if configs.len() == 1 {
                    configs.push(other_configs[0]);
                }

                configs
                    .into_iter()
                    .map(|filename| {
                        Box::new(DotfileShellIntegration {
                            pre: true,
                            post: true,
                            shell: *self,
                            dotfile_directory: config_dir.clone(),
                            dotfile_name: filename,
                        }) as Box<dyn ShellIntegration>
                    })
                    .collect()
            },
            Shell::Zsh => vec![".zshrc", ".zprofile"]
                .into_iter()
                .map(|filename| {
                    Box::new(DotfileShellIntegration {
                        pre: true,
                        post: true,
                        shell: *self,
                        dotfile_directory: config_dir.clone(),
                        dotfile_name: filename,
                    }) as Box<dyn ShellIntegration>
                })
                .collect(),
            Shell::Fish => {
                let fish_config_dir = config_dir.join("conf.d");
                vec![
                    Box::new(ShellScriptShellIntegration {
                        when: When::Pre,
                        shell: *self,
                        path: fish_config_dir.join("00_fig_pre.fish"),
                    }),
                    Box::new(ShellScriptShellIntegration {
                        when: When::Post,
                        shell: *self,
                        path: fish_config_dir.join("99_fig_post.fish"),
                    }),
                ]
            },
            Shell::Nu => vec![],
        };

        Ok(integrations)
    }

    fn get_fig_integration_source(&self, when: &When) -> String {
        let script = match (self, when) {
            (Shell::Fish, When::Pre) => include_str!("scripts/pre.fish"),
            (Shell::Fish, When::Post) => include_str!("scripts/post.fish"),
            (Shell::Zsh, When::Pre) => include_str!("scripts/pre.sh"),
            (Shell::Zsh, When::Post) => include_str!("scripts/post.zsh"),
            (Shell::Bash, When::Pre) => {
                concat!(
                    "function __fig_source_bash_preexec() {\n",
                    include_str!("scripts/bash-preexec.sh"),
                    "}\n",
                    "__fig_source_bash_preexec\n",
                    "# shellcheck disable=SC2329\n",
                    "function __bp_adjust_histcontrol() { :; }\n",
                    include_str!("scripts/pre.sh")
                )
            },
            (Shell::Bash, When::Post) => {
                concat!(
                    "function __fig_source_bash_preexec() {\n",
                    include_str!("scripts/bash-preexec.sh"),
                    "}\n",
                    "__fig_source_bash_preexec\n",
                    "# shellcheck disable=SC2329\n",
                    "function __bp_adjust_histcontrol() { :; }\n",
                    include_str!("scripts/post.bash")
                )
            },
            (Shell::Nu, When::Pre) => include_str!("scripts/pre.nu"),
            (Shell::Nu, When::Post) => include_str!("scripts/post.nu"),
        };

        script
            .replace("{{CLI_BINARY_NAME}}", CLI_BINARY_NAME)
            .replace("{{PTY_BINARY_NAME}}", PTY_BINARY_NAME)
            .replace("{{PTY_BINARY_PATH}}", &quoted_pty_binary_path(*self))
    }
}

pub trait ShellIntegration: Send + Sync + Integration + ShellIntegrationClone {
    // The unique name of the integration file
    fn file_name(&self) -> &str;
    fn get_shell(&self) -> Shell;
    fn path(&self) -> PathBuf;
}

impl std::fmt::Display for dyn ShellIntegration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.get_shell(), self.path().display())
    }
}

pub trait ShellIntegrationClone {
    fn clone_box(&self) -> Box<dyn ShellIntegration>;
}

impl<T> ShellIntegrationClone for T
where
    T: 'static + ShellIntegration + Clone,
{
    fn clone_box(&self) -> Box<dyn ShellIntegration> {
        Box::new(self.clone())
    }
}

// We can now implement Clone manually by forwarding to clone_box.
impl Clone for Box<dyn ShellIntegration> {
    fn clone(&self) -> Box<dyn ShellIntegration> {
        self.clone_box()
    }
}

#[derive(Debug, Clone)]
pub struct ShellScriptShellIntegration {
    pub shell: Shell,
    pub when: When,
    pub path: PathBuf,
}

/// The variable through which `init <shell> pre` hands the matching post
/// script to the post shim of the same rc file, so a shell start runs the
/// CLI once per rc file instead of twice. zsh and bash shims read it.
pub fn post_handoff_variable(rcfile: Option<&str>) -> String {
    let name: String = rcfile
        .unwrap_or("rc")
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    format!("__fastab_post_{name}")
}

fn get_prefix(s: &str) -> &str {
    match s.find('.') {
        Some(i) => &s[..i],
        None => s,
    }
}

fn looks_like_sibling_shell_file(text: &str) -> bool {
    text.lines().any(is_sibling_hook_line)
}

fn looks_like_fastab_shell_file(text: &str) -> bool {
    text.lines().any(is_fastab_shell_line)
}

fn is_fastab_shell_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with('#') {
        let lower = t.to_ascii_lowercase();
        return lower.starts_with("# fastab ") || lower.starts_with("# fastab\t");
    }
    t.contains("ftab init")
        || t.contains("/.local/bin/ftab")
        || t.contains("command -v ftab >/dev/null")
        || t.contains("command -qv ftab")
        || t.contains("fastab/shell/")
}

fn is_sibling_hook_line(line: &str) -> bool {
    if is_fastab_shell_line(line) {
        return false;
    }
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if t.starts_with('#') {
        return lower.contains("easy complete")
            || lower.contains("easy-complete")
            || lower.contains("amazon q")
            || (lower.contains("fig") && (lower.contains("pre block") || lower.contains("post block")))
            || (lower.contains("codewhisperer") && (lower.contains("pre block") || lower.contains("post block")));
    }
    t.contains("ec init")
        || t.contains("/.local/bin/ec")
        || t.contains("command -v ec >/dev/null")
        || t.contains("command -qv ec")
        || t.contains("easy-complete/shell")
        || t.contains("q init")
        || t.contains("/.local/bin/q")
        || t.contains("command -v q >/dev/null")
        || t.contains("command -qv q")
        || t.contains("fig init")
        || t.contains("/.local/bin/fig")
        || t.contains("command -v fig >/dev/null")
        || t.contains("command -qv fig")
        || t.contains(".fig/shell")
        || t.contains("codewhisperer/shell")
}

fn is_local_bin_path_line(line: &str) -> bool {
    if is_fastab_shell_line(line) {
        return false;
    }
    let t = line.trim();
    let has_local_bin = t.contains(".local/bin");
    if !has_local_bin {
        return false;
    }
    t.contains("PATH") || t.contains("fish_user_paths") || t.contains("contains $HOME/.local/bin")
}

fn is_sibling_preamble_line(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || is_sibling_hook_line(line) || is_local_bin_path_line(line)
}

impl ShellScriptShellIntegration {
    fn get_file_integration(&self) -> FileIntegration {
        FileIntegration {
            path: self.path.clone(),
            contents: self.get_contents(),
            #[cfg(unix)]
            mode: None,
        }
    }

    fn get_name(&self) -> Option<&str> {
        self.path.file_name().and_then(|s| s.to_str())
    }

    #[allow(clippy::needless_return)]
    fn get_contents(&self) -> String {
        let Self { shell, when, path } = self;
        let prefix = path.file_name().and_then(|x| x.to_str()).map(get_prefix);
        let rcfile = match prefix {
            Some(name) => format!(" --rcfile {name}"),
            None => "".into(),
        };
        cfg_if!(
            if #[cfg(target_os = "macos")] {
                // The pre run usually hands over the post script already; the
                // CLI only runs again when it did not (an older pre, or an
                // input method prompt that the post run has to count).
                let handoff = post_handoff_variable(prefix);
                return match (self.shell, when) {
                    // Check if ~/.local/bin/{CLI_BINARY_NAME} is executable before eval
                    (Shell::Bash | Shell::Zsh, When::Pre) => format!("[ -x ~/.local/bin/{CLI_BINARY_NAME} ] && eval \"$(~/.local/bin/{CLI_BINARY_NAME} init {shell} {when}{rcfile})\""),
                    (Shell::Bash | Shell::Zsh, When::Post) => format!("if [ -n \"${{{handoff}+x}}\" ]; then eval \"${handoff}\"; unset {handoff}; elif [ -x ~/.local/bin/{CLI_BINARY_NAME} ]; then eval \"$(~/.local/bin/{CLI_BINARY_NAME} init {shell} {when}{rcfile})\"; fi"),
                    (Shell::Fish, _) => format!("test -x ~/.local/bin/{CLI_BINARY_NAME}; and eval (~/.local/bin/{CLI_BINARY_NAME} init {shell} {when}{rcfile} | string split0)"),
                    (Shell::Nu, _) => "".into(),
                }
            } else {
                let add_to_path_line = match self.shell {
                    Shell::Bash | Shell::Zsh => indoc::indoc! {r#"
                        _Q_LOCAL_BIN="$HOME/.local/bin"
                        [[ ":$PATH:" != *":$_Q_LOCAL_BIN:"* ]] && PATH="${PATH:+"$PATH:"}$_Q_LOCAL_BIN"
                        unset _Q_LOCAL_BIN
                    "#},
                    Shell::Fish => "contains $HOME/.local/bin $PATH; or set -a PATH $HOME/.local/bin",
                    Shell::Nu => "",
                };

                let source_line = match self.shell {
                    Shell::Fish => format!("command -qv {CLI_BINARY_NAME}; and eval ({CLI_BINARY_NAME} init {shell} {when}{rcfile} | string split0)"),
                    Shell::Bash | Shell::Zsh => {
                        // Check that the current shell is bash
                        let bash_pre = if self.shell.is_bash() { "[ -n \"$BASH_VERSION\" ] && " } else { "" };
                        format!("{bash_pre}command -v {CLI_BINARY_NAME} >/dev/null 2>&1 && eval \"$({CLI_BINARY_NAME} init {shell} {when}{rcfile})\"")
                    }
                    Shell::Nu => "".into(),
                };

                return format!("{add_to_path_line}\n{source_line}\n");
            }
        );
    }
}

#[async_trait]
impl Integration for ShellScriptShellIntegration {
    async fn is_installed(&self) -> Result<()> {
        let file = self.get_file_integration();
        match file.is_installed().await {
            Ok(()) => Ok(()),
            Err(Error::ImproperInstallation(_)) => {
                let Ok(existing) = tokio::fs::read_to_string(&self.path).await else {
                    return file.is_installed().await;
                };
                if existing.contains(&self.get_contents()) {
                    return Ok(());
                }
                file.is_installed().await
            },
            Err(err) => Err(err),
        }
    }

    async fn install(&self) -> Result<()> {
        if self.is_installed().await.is_ok() {
            return Ok(());
        }
        if let Ok(existing) = tokio::fs::read_to_string(&self.path).await {
            if looks_like_sibling_shell_file(&existing) {
                let ours = self.get_contents();
                if existing.contains(&ours) {
                    return Ok(());
                }
                let mut combined = existing;
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&ours);
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                tokio::fs::write(&self.path, combined).await.with_path(&self.path)?;
                return Ok(());
            }
        }
        self.get_file_integration().install().await
    }

    async fn uninstall(&self) -> Result<()> {
        if let Ok(existing) = tokio::fs::read_to_string(&self.path).await {
            if looks_like_sibling_shell_file(&existing) {
                if !looks_like_fastab_shell_file(&existing) {
                    return Ok(());
                }
                let kept: String = existing
                    .lines()
                    .filter(|line| !is_fastab_shell_line(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let kept = kept.trim_end();
                if kept.is_empty() {
                    return self.get_file_integration().uninstall().await;
                }
                let mut kept = kept.to_string();
                kept.push('\n');
                tokio::fs::write(&self.path, kept).await.with_path(&self.path)?;
                return Ok(());
            }
        }
        self.get_file_integration().uninstall().await
    }

    fn describe(&self) -> String {
        format!("{} {}", self.shell, self.when)
    }

    async fn migrate(&self) -> Result<()> {
        // FileIntegration::install truncates. Shared fish names
        // (`00_fig_pre.fish`) must append beside Easy Complete / Amazon Q.
        self.install().await
    }
}

impl ShellIntegration for ShellScriptShellIntegration {
    fn file_name(&self) -> &str {
        self.get_name().unwrap_or("unknown_script")
    }

    fn get_shell(&self) -> Shell {
        self.shell
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

/// zsh and bash integration where we modify a dotfile with pre/post hooks that reference script
/// files.
#[derive(Debug, Clone)]
pub struct DotfileShellIntegration {
    pub shell: Shell,
    pub pre: bool,
    pub post: bool,
    pub dotfile_directory: PathBuf,
    pub dotfile_name: &'static str,
}

impl DotfileShellIntegration {
    fn dotfile_path(&self) -> PathBuf {
        self.dotfile_directory.join(self.dotfile_name)
    }

    fn script_integration(&self, when: When) -> Result<ShellScriptShellIntegration> {
        let integration_file_name = format!(
            "{}.{}.{}",
            Regex::new(r"^\.").unwrap().replace_all(self.dotfile_name, ""),
            when,
            self.shell
        );
        Ok(ShellScriptShellIntegration {
            shell: self.shell,
            when,
            path: directories::fig_data_dir()?.join("shell").join(integration_file_name),
        })
    }

    #[allow(clippy::unused_self)]
    fn description(&self, when: When) -> String {
        match when {
            When::Pre => format!("# {INTEGRATION_MARKER_NAME} pre block. Keep at the top of this file."),
            When::Post => format!(
                // "Near" not "at": Otty (and similar) may own the absolute end of bashrc.
                // See strip_trailing_foreign_integrations / Otty shell-integration docs.
                "# {INTEGRATION_MARKER_NAME} post block. Keep near the bottom of this file."
            ),
        }
    }

    /// Fastab-owned leftover styles only. Fig `~/.fig` sources and Amazon Q /
    /// CodeWhisperer blocks stay in the rc — those products can sit beside us.
    fn leftover_regexes(&self, when: When) -> Result<RegexSet> {
        let comment = regex::escape(&self.description(when));
        let shell = self.shell;

        let eval_line = match shell {
            Shell::Fish => format!("eval ({CLI_BINARY_NAME} init {shell} {when} | string split0)"),
            _ => format!("eval \"$({CLI_BINARY_NAME} init {shell} {when})\""),
        };

        let old_eval_source = match when {
            When::Pre => match self.shell {
                Shell::Fish => format!("set -Ua fish_user_paths $HOME/.local/bin\n{eval_line}"),
                _ => format!("export PATH=\"${{PATH}}:${{HOME}}/.local/bin\"\n{eval_line}"),
            },
            When::Post => eval_line,
        };

        let old_eval_regex = format!(r#"(?m)(?:{comment}\n)?^{}\n{{0,2}}"#, regex::escape(&old_eval_source),);
        let old_source_regex_1 = format!(
            r#"(?m)(?:{comment}\n)?^{}\n{{0,2}}"#,
            regex::escape(&self.legacy_source_text_1(when)?),
        );
        let old_source_regex_2 = format!(
            r#"(?m)(?:{comment}\n)?^{}\n{{0,2}}"#,
            regex::escape(&self.legacy_source_text_2(when)?),
        );

        Ok(RegexSet::new([
            old_eval_regex,
            old_source_regex_1,
            old_source_regex_2,
            self.leftover_eval_line_regex(when),
        ])?)
    }

    /// Standalone `ftab init` eval lines copied into a zshrc/bashrc (manual
    /// install or a stub pasted out of `fastab/shell`). Does not match `ec` /
    /// `q` / `fig` init.
    fn leftover_eval_line_regex(&self, when: When) -> String {
        let comment = regex::escape(&self.description(when));
        let bin = regex::escape(CLI_BINARY_NAME);
        let shell = regex::escape(&self.shell.to_string());
        let when_s = regex::escape(&when.to_string());
        match self.shell {
            Shell::Fish => format!(
                r#"(?m)(?:{comment}\n)?^(?:test -x ~/\.local/bin/{bin}; and )?eval \((?:~/\.local/bin/)?{bin} init {shell} {when_s}[^\n)]*\| string split0\)\n{{0,2}}"#
            ),
            _ => format!(
                r#"(?m)(?:{comment}\n)?^(?:\[ -x ~/\.local/bin/{bin} \] && |command -v {bin} >/dev/null 2>&1 && )?eval "\$\((?:~/\.local/bin/)?{bin} init {shell} {when_s}[^\n"]*\)"\n{{0,2}}"#
            ),
        }
    }

    fn legacy_source_text_1(&self, when: When) -> Result<String> {
        let home = directories::home_dir()?;
        let integration_path = self.script_integration(when)?.path;
        let path = integration_path.strip_prefix(home)?;
        Ok(format!(". \"$HOME/{}\"", path.display()))
    }

    fn legacy_source_text_2(&self, when: When) -> Result<String> {
        let home = directories::home_dir()?;
        let integration_path = self.script_integration(when)?.path;
        let path = format!("\"$HOME/{}\"", integration_path.strip_prefix(home)?.display());

        match self.shell {
            Shell::Fish => Ok(format!("if test -f {path}; . {path}; end")),
            _ => Ok(format!("[[ -f {path} ]] && . {path}")),
        }
    }

    fn source_text(&self, when: When) -> Result<String> {
        let home = directories::home_dir()?;
        let integration_path = self.script_integration(when)?.path;
        let path = format!("\"${{HOME}}/{}\"", integration_path.strip_prefix(home)?.display());

        match self.shell {
            Shell::Fish => Ok(format!("test -f {path}; and builtin source {path}")),
            _ => Ok(format!("[[ -f {path} ]] && builtin source {path}")),
        }
    }

    fn source_regex(&self, when: When, constrain_position: bool) -> Result<Regex> {
        let regex = format!(
            r#"{}(?:{}\n)?{}\n{{0,2}}{}"#,
            if constrain_position && when == When::Pre {
                "^"
            } else {
                ""
            },
            regex::escape(&self.description(when)),
            regex::escape(&self.source_text(when)?),
            if constrain_position && when == When::Post {
                "$"
            } else {
                ""
            },
        );
        Ok(Regex::new(&regex)?)
    }

    fn remove_from_text(&self, text: impl Into<String>, when: When) -> Result<String> {
        let source_regex = self.source_regex(when, false)?;
        let mut regexes = vec![source_regex];
        regexes.extend(
            self.leftover_regexes(when)?
                .patterns()
                .iter()
                .map(|r| Regex::new(r).unwrap()),
        );
        Ok(regexes
            .iter()
            .fold::<String, _>(text.into(), |acc, reg| reg.replace_all(&acc, "").into()))
    }

    fn matches_text(&self, text: &str, when: When) -> Result<()> {
        let dotfile = self.dotfile_path();
        if self.leftover_regexes(when)?.is_match(text) {
            let message = format!("{} has legacy {} integration.", dotfile.display(), when);
            return Err(Error::LegacyInstallation(message.into()));
        }
        if !self.source_regex(when, false)?.is_match(text) {
            let message = format!("{} does not source {} integration", dotfile.display(), when);
            return Err(Error::NotInstalled(message.into()));
        }

        // Position rules differ for pre vs post:
        //
        // - Pre must stay first so ecterm can wrap the shell early.
        // - Post used to require being last. That fights Otty: for bash, Otty
        //   documents that it appends a managed block to ~/.bashrc and rewrites
        //   it to the absolute end whenever Shell Integration is (re)enabled /
        //   the app launches (https://docs.otty.sh/terminal-features/shell-integration).
        //   The block is inert unless $OTTY_SHELL_INTEGRATION is set, so it does
        //   not break our hooks — but a strict "must be last" check makes Settings
        //   forever report "needs setup" and repair loops against Otty's installer.
        //   When checking post position, strip known inert foreign trailers first.
        // Dual-install: Easy Complete (and Amazon Q) use the same first/last
        // rule. Treat their blocks as inert for position so repair does not
        // ping-pong with the sibling installer.
        let text_for_position = match when {
            When::Pre => strip_leading_sibling_pre_blocks(text),
            When::Post => {
                let without_foreign = strip_trailing_foreign_integrations(text);
                strip_trailing_sibling_post_blocks(&without_foreign)
            },
        };
        if !self.source_regex(when, true)?.is_match(&text_for_position) {
            let position = match when {
                When::Pre => "first",
                When::Post => "last",
            };
            let message = format!(
                "{} does not source {} integration {}",
                dotfile.display(),
                when,
                position
            );
            return Err(Error::ImproperInstallation(message.into()));
        }
        Ok(())
    }

    async fn install_inner(&self) -> Result<()> {
        let dotfile = self.dotfile_path();
        let mut contents = if dotfile.exists() {
            backup_file(&dotfile, fastab_util::directories::utc_backup_dir().ok())?;
            self.uninstall().await?;
            std::fs::read_to_string(&dotfile)?
        } else {
            String::new()
        };

        let original_contents = contents.clone();

        if self.pre {
            self.script_integration(When::Pre)?.install().await?;
            let (shebang, post_shebang) = split_shebang(&contents);
            let (sibling_lead, rest) = split_leading_sibling_pre_blocks(post_shebang);
            let mut assembled = String::new();
            assembled.push_str(shebang);
            assembled.push_str(&sibling_lead);
            if !sibling_lead.is_empty() && !sibling_lead.ends_with('\n') {
                assembled.push('\n');
            }
            assembled.push_str(&self.description(When::Pre));
            assembled.push('\n');
            assembled.push_str(&self.source_text(When::Pre)?);
            assembled.push('\n');
            assembled.push_str(&rest);
            contents = assembled;
        }

        if self.post {
            self.script_integration(When::Post)?.install().await?;
            // Sit just above foreign trailers instead of appending past them.
            // Otty (bash) always wants its block at the absolute end; if repair
            // shoved post below Otty, the next Otty launch would move Otty back
            // under us and Settings would flake again. Leaving Otty's trailer
            // untouched keeps both installers stable.
            // Docs: https://docs.otty.sh/terminal-features/shell-integration
            let (body, trailing) = split_trailing_foreign_integrations(&contents);
            contents = format!(
                "{}\n{}\n{}\n{}",
                body.trim_end(),
                self.description(When::Post),
                self.source_text(When::Post)?,
                trailing,
            );
        }

        if contents.ne(&original_contents) {
            let mut file = File::create(&dotfile).with_path(self.path())?;
            file.write_all(contents.as_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl Integration for DotfileShellIntegration {
    fn describe(&self) -> String {
        format!(
            "{}{}{} into {}",
            self.shell,
            if self.pre { " pre" } else { "" },
            if self.post { " post" } else { "" },
            self.dotfile_name,
        )
    }

    async fn install(&self) -> Result<()> {
        if self.is_installed().await.is_ok() {
            return Ok(());
        }
        self.install_inner().await?;
        // A rewrite the status check still rejects must not read as a successful install.
        self.is_installed().await
    }

    async fn uninstall(&self) -> Result<()> {
        let dotfile = self.dotfile_path();
        if dotfile.exists() {
            let mut contents = std::fs::read_to_string(&dotfile)?;

            if self.pre {
                contents = self.remove_from_text(&contents, When::Pre)?;
            }

            if self.post {
                contents = self.remove_from_text(&contents, When::Post)?;
            }

            contents = contents.trim_end().to_string();
            contents.push('\n');

            std::fs::write(&dotfile, contents.as_bytes()).with_path(self.path())?;
        }

        if self.pre {
            self.script_integration(When::Pre)?.uninstall().await?;
        }

        if self.post {
            self.script_integration(When::Post)?.uninstall().await?;
        }

        Ok(())
    }

    async fn is_installed(&self) -> Result<()> {
        let dotfile = self.dotfile_path();

        let filtered_contents: String = match std::fs::read_to_string(&dotfile).with_path(&dotfile) {
            // Remove comments and empty lines.
            Ok(contents) => {
                // Check for existence of ignore flag
                if Regex::new(r"(?mi)^\s*#\s*fig ignore\s?.*$")
                    .unwrap()
                    .is_match(&contents)
                {
                    return Ok(());
                }

                Regex::new(r"(?m)^\s*(#.*)?\n")
                    .unwrap()
                    .replace_all(&contents, "")
                    .into()
            },
            Err(Error::Io(err)) if err.kind() == ErrorKind::NotFound => {
                return Err(Error::FileDoesNotExist(dotfile.into()));
            },
            Err(err) => return Err(err),
        };

        let filtered_contents = filtered_contents.trim();

        if self.pre {
            self.matches_text(filtered_contents, When::Pre)?;
            self.script_integration(When::Pre)?.is_installed().await?;
        }

        if self.post {
            self.matches_text(filtered_contents, When::Post)?;
            self.script_integration(When::Post)?.is_installed().await?;
        }

        Ok(())
    }

    async fn migrate(&self) -> Result<()> {
        match self.is_installed().await {
            Ok(_) => Ok(()),
            Err(Error::LegacyInstallation(_)) => {
                self.install_inner().await?;
                Ok(())
            },
            Err(err) => Err(err),
        }
    }
}

impl ShellIntegration for DotfileShellIntegration {
    fn get_shell(&self) -> Shell {
        self.shell
    }

    fn path(&self) -> PathBuf {
        self.dotfile_path()
    }

    fn file_name(&self) -> &str {
        self.dotfile_name
    }
}

/// Splits the line containing the shebang (if any) with the rest of the string.
/// If the shebang exists, the newline is included. Otherwise, an empty slice is returned.
fn split_shebang(contents: &str) -> (&str, &str) {
    if contents.starts_with("#!") {
        match contents.find('\n') {
            Some(i) => (&contents[..i + 1], &contents[i + 1..]),
            None => ("", contents),
        }
    } else {
        ("", contents)
    }
}

/// Third-party shell hooks that claim the absolute end of a dotfile.
///
/// Today this is Otty's bash (and tmux-managed zsh) integration:
/// https://docs.otty.sh/terminal-features/shell-integration
///
/// Otty's block is guarded on `$OTTY_SHELL_INTEGRATION`, so it is a no-op in
/// every other terminal. It is *not* Otty Autocomplete (a separate Fig-compatible
/// UI); coexistence here only means "don't treat their rc trailer as a broken
/// Fastab install."
///
/// Otty 1.6 also appends a `bash-profile shim` to `~/.bash_profile` that sources
/// `~/.bashrc`; the `.bashrc` post block still runs after it, so our hooks stay last.
///
/// Matched against both:
/// - raw rc files (with `# >>> otty … >>>` markers), and
/// - the comment-/blank-stripped form produced by [`DotfileShellIntegration::is_installed`].
///
/// Otty has changed the guard of its `if` between releases (1.6 added
/// `[ -n "$BASH_VERSION" ] &&`), so the stripped forms key on what the block
/// sources, not on the exact condition.
fn trailing_foreign_integration_patterns() -> &'static [&'static str] {
    &[
        // Otty — marker form written into ~/.bashrc / ~/.bash_profile (and tmux-managed zsh/fish rc).
        // Only blocks the stripped forms below also recognise, so install and status agree.
        // The 1.6 bash-profile shim closes with `# <<< otty bash-profile shim >>>`.
        r"(?ms)\n*#\s*>>> otty (?:shell integration|bash-profile shim) >>>.*?#\s*<<< otty (?:shell integration|bash-profile shim) (?:<<<|>>>)\s*",
        // Otty shell integration — after is_installed strips comment-only / empty
        // lines, only the guarded `if … OTTY_SHELL_INTEGRATION …; fi` body remains.
        r#"(?m)\n*^if [^\n]*"\$OTTY_SHELL_INTEGRATION"[^\n]*;\s*then\n\s*\.\s+"\$OTTY_SHELL_INTEGRATION/otty-integration\.(?:bash|zsh)"\nfi\s*"#,
        // Otty bash-profile shim, stripped form.
        r#"(?m)\n*^if\s+\[\s+-n\s+"\$BASH_VERSION"\s*\]\s*&&\s*\[\s+-f\s+"\$HOME/\.bashrc"\s*\]\s*;\s*then\s+\.\s+"\$HOME/\.bashrc"\s*;\s*fi\s*"#,
    ]
}

fn foreign_integration_regexes() -> Vec<Regex> {
    trailing_foreign_integration_patterns()
        .iter()
        .map(|p| Regex::new(p).expect("foreign integration regex"))
        .collect()
}

/// Peel leading Easy Complete / Amazon Q pre blocks (and their PATH lines)
/// so Fastab's "must be first" check does not fight the sibling installer.
fn split_leading_sibling_pre_blocks(text: &str) -> (String, String) {
    let mut consumed = 0usize;
    let mut peeled_any = false;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if is_sibling_preamble_line(content) {
            consumed += line.len();
            peeled_any = true;
        } else {
            break;
        }
    }
    if !peeled_any {
        return (String::new(), text.to_owned());
    }
    (text[..consumed].to_owned(), text[consumed..].to_owned())
}

fn strip_leading_sibling_pre_blocks(text: &str) -> String {
    split_leading_sibling_pre_blocks(text).1
}

/// Peel trailing sibling post blocks so Fastab's "must be last" check accepts
/// either order versus Easy Complete / Amazon Q.
fn strip_trailing_sibling_post_blocks(text: &str) -> String {
    let mut end = text.len();
    for line in text.split_inclusive('\n').rev() {
        let content = line.trim_end_matches(['\n', '\r']);
        if is_sibling_preamble_line(content) {
            end -= line.len();
        } else {
            break;
        }
    }
    text[..end].trim_end().to_owned()
}

/// Drop known third-party trailers from the end of `text` (repeat until stable).
/// Used so post's "must be last" position check ignores Otty's managed block.
fn strip_trailing_foreign_integrations(text: &str) -> String {
    let mut result = text.trim_end().to_owned();
    let patterns = foreign_integration_regexes();

    loop {
        let before_len = result.len();
        for re in &patterns {
            if let Some(m) = re.find_iter(&result).last() {
                // Only peel matches that sit at EOF — never mid-file copies.
                if m.end() == result.len() {
                    result.truncate(m.start());
                    let trimmed = result.trim_end();
                    result.truncate(trimmed.len());
                }
            }
        }
        if result.len() == before_len {
            break;
        }
    }
    result
}

/// Split `(body, trailing_foreign)` so install/repair can place our post block
/// *above* Otty's trailer without rewriting or deleting Otty's installer output.
/// `trailing_foreign` keeps the exact suffix from `contents` (blank lines included).
fn split_trailing_foreign_integrations(contents: &str) -> (String, String) {
    let trimmed = contents.trim_end();
    let stripped = strip_trailing_foreign_integrations(trimmed);
    if stripped.len() == trimmed.len() {
        return (contents.to_owned(), String::new());
    }

    // `stripped` is always a prefix of `trimmed` because we only truncate from the end
    // (then trim whitespace that sat between our block and the foreign trailer).
    if let Some(rest) = trimmed.strip_prefix(&stripped) {
        if rest.is_empty() {
            return (contents.to_owned(), String::new());
        }
        let cut = stripped.len();
        // Prefer cutting inside `contents` at the same prefix length when contents starts
        // with stripped; otherwise fall back to the trimmed view.
        if contents.starts_with(&stripped) {
            return (contents[..cut].to_owned(), contents[cut..].to_owned());
        }
        return (stripped, rest.to_owned());
    }

    (contents.to_owned(), String::new())
}

#[cfg(test)]
mod test {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use fastab_util::build::SKIP_SHELLCHECK_TESTS;
    use fastab_util::directories::{home_dir, old_fig_data_dir, previous_product_data_dir};

    use super::*;

    fn run_shellcheck(source: String) {
        if SKIP_SHELLCHECK_TESTS {
            return;
        }

        let shell_arg = "--shell=bash";
        let mut child = Command::new("shellcheck")
            .args([shell_arg, "--color=always", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            stdin.write_all(source.as_bytes()).unwrap();
        });

        let output = child.wait_with_output().unwrap();
        if !output.status.success() {
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();

            if !stdout.is_empty() {
                println!("{stdout}");
            }

            if !stderr.is_empty() {
                eprintln!("{stderr}");
            }

            if stdout.contains("error") {
                panic!();
            }
        }
    }

    fn check_script(shell: Shell, when: When) {
        run_shellcheck(shell.get_fig_integration_source(&when));
    }

    #[test]
    fn shellcheck_bash_pre() {
        check_script(Shell::Bash, When::Pre);
    }

    #[test]
    fn shellcheck_bash_post() {
        check_script(Shell::Bash, When::Post);
    }

    fn zshrc_integration() -> DotfileShellIntegration {
        DotfileShellIntegration {
            pre: true,
            post: true,
            shell: Shell::Zsh,
            dotfile_directory: "".into(),
            dotfile_name: ".zshrc",
        }
    }

    fn fish_integration() -> DotfileShellIntegration {
        DotfileShellIntegration {
            pre: true,
            post: true,
            shell: Shell::Fish,
            dotfile_directory: "".into(),
            dotfile_name: "config.fish",
        }
    }

    #[test]
    fn test_easy_complete_blocks_are_left_alone() {
        let integration = zshrc_integration();
        let data_dir = previous_product_data_dir().unwrap();
        let dir = data_dir.strip_prefix(home_dir().unwrap()).unwrap().display();
        let source = format!(
            r#"[[ -f "${{HOME}}/{dir}/shell/zshrc.pre.zsh" ]] && builtin source "${{HOME}}/{dir}/shell/zshrc.pre.zsh""#
        );
        let doc = format!("# Easy Complete pre block. Keep at the top of this file.\n{source}\nexport PATH=/usr/bin\n");

        let stripped = integration.remove_from_text(&doc, When::Pre).unwrap();
        assert!(
            stripped.contains("Easy Complete"),
            "sibling Easy Complete comment must stay: {stripped}"
        );
        assert!(
            stripped.contains("easy-complete/shell"),
            "sibling Easy Complete source must stay: {stripped}"
        );
        assert!(
            !integration.leftover_regexes(When::Pre).unwrap().is_match(&doc),
            "Easy Complete blocks must not look like a Fastab leftover"
        );
    }

    #[test]
    fn test_codewhisperer_blocks_are_left_alone() {
        let integration = zshrc_integration();
        let data_dir = old_fig_data_dir().unwrap();
        let dir = data_dir.strip_prefix(home_dir().unwrap()).unwrap().display();
        let source = format!(
            r#"[[ -f "${{HOME}}/{dir}/shell/zshrc.pre.zsh" ]] && builtin source "${{HOME}}/{dir}/shell/zshrc.pre.zsh""#
        );
        let doc = format!("# CodeWhisperer pre block. Keep at the top of this file.\n{source}\nexport KEEP=/usr/bin\n");

        let stripped = integration.remove_from_text(&doc, When::Pre).unwrap();
        assert!(
            stripped.contains("CodeWhisperer"),
            "Amazon Q comment must stay: {stripped}"
        );
        assert!(
            stripped.contains(&format!("{dir}/shell")),
            "Amazon Q source must stay: {stripped}"
        );
        assert!(
            !integration.leftover_regexes(When::Pre).unwrap().is_match(&doc),
            "Amazon Q blocks must not look like a Fastab leftover"
        );
    }

    #[test]
    fn test_fig_dotfile_source_is_left_alone() {
        let integration = zshrc_integration();
        let doc = "[ -s ~/.fig/shell/pre.sh ] && source ~/.fig/shell/pre.sh\nexport KEEP=/usr/bin\n";
        let stripped = integration.remove_from_text(doc, When::Pre).unwrap();
        assert!(
            stripped.contains("~/.fig/shell/pre.sh"),
            "Fig ~/.fig source must stay: {stripped}"
        );
        assert!(
            !integration.leftover_regexes(When::Pre).unwrap().is_match(doc),
            "Fig ~/.fig source must not look like a Fastab leftover"
        );
    }

    #[test]
    fn test_q_and_fig_init_eval_are_left_alone() {
        let integration = zshrc_integration();
        let lines = [
            r#"eval "$(q init zsh pre)""#,
            r#"eval "$(fig init zsh pre)""#,
            r#"[ -x ~/.local/bin/q ] && eval "$(~/.local/bin/q init zsh pre --rcfile zshrc)""#,
            r#"[ -x ~/.local/bin/fig ] && eval "$(~/.local/bin/fig init zsh pre --rcfile zshrc)""#,
        ];
        for line in lines {
            let stripped = integration.remove_from_text(line, When::Pre).unwrap();
            assert!(
                stripped.contains("init"),
                "sibling init eval must stay: {line} -> {stripped}"
            );
            assert!(
                !integration.leftover_regexes(When::Pre).unwrap().is_match(line),
                "sibling init eval must not look like a Fastab leftover: {line}"
            );
        }
    }

    #[test]
    fn test_ec_init_eval_is_left_alone() {
        let integration = zshrc_integration();
        let lines = [
            r#"eval "$(ec init zsh pre)""#,
            r#"eval "$(ec init zsh pre --rcfile zshrc)""#,
            r#"command -v ec >/dev/null 2>&1 && eval "$(ec init zsh pre --rcfile zshrc)""#,
            r#"[ -x ~/.local/bin/ec ] && eval "$(~/.local/bin/ec init zsh pre --rcfile zshrc)""#,
            r#"export PATH="${PATH}:${HOME}/.local/bin"
eval "$(ec init zsh pre --rcfile zshrc)""#,
        ];
        for line in lines {
            let doc =
                format!("# Easy Complete pre block. Keep at the top of this file.\n{line}\nexport KEEP=/usr/bin\n");
            let stripped = integration.remove_from_text(&doc, When::Pre).unwrap();
            assert!(
                stripped.contains("ec init"),
                "sibling ec init must stay: {line} -> {stripped}"
            );
            assert!(
                !integration.leftover_regexes(When::Pre).unwrap().is_match(&doc),
                "sibling ec init must not look like a Fastab leftover: {line}"
            );
        }
    }

    #[test]
    fn test_leftover_ftab_eval_is_removed() {
        let integration = zshrc_integration();
        let lines = [
            r#"[ -x ~/.local/bin/ftab ] && eval "$(~/.local/bin/ftab init zsh pre --rcfile zshrc)""#,
            r#"eval "$(ftab init zsh pre)""#,
            "export PATH=\"${PATH}:${HOME}/.local/bin\"\neval \"$(ftab init zsh pre)\"\n",
        ];
        for line in lines {
            let stripped = integration.remove_from_text(line, When::Pre).unwrap();
            assert!(
                !stripped.contains("ftab init"),
                "leftover Fastab eval must be removed: {line} -> {stripped}"
            );
        }
    }

    #[test]
    fn test_ec_init_fish_is_left_alone() {
        let integration = fish_integration();
        let lines = [
            r#"eval (ec init fish pre | string split0)"#,
            r#"eval (ec init fish pre --rcfile config | string split0)"#,
            r#"test -x ~/.local/bin/ec; and eval (~/.local/bin/ec init fish pre --rcfile config | string split0)"#,
        ];
        for line in lines {
            let doc =
                format!("# Easy Complete pre block. Keep at the top of this file.\n{line}\nset -gx KEEP /usr/bin\n");
            let stripped = integration.remove_from_text(&doc, When::Pre).unwrap();
            assert!(
                stripped.contains("ec init"),
                "sibling fish ec init must stay: {line} -> {stripped}"
            );
        }
    }

    #[tokio::test]
    async fn test_fish_script_install_preserves_easy_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(
            &path,
            "# Easy Complete pre block\neval (ec init fish pre | string split0)\n",
        )
        .unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.install().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(contents.contains("ec init"), "sibling fish hook must stay: {contents}");
        assert!(
            contents.contains("ftab init"),
            "Fastab fish hook must be added: {contents}"
        );

        integration.uninstall().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(
            contents.contains("ec init"),
            "uninstall must leave sibling fish hook: {contents}"
        );
        assert!(
            !contents.contains("ftab"),
            "uninstall must drop Fastab fish hook: {contents}"
        );
    }

    #[tokio::test]
    async fn test_fish_script_install_preserves_q_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(&path, "eval (q init fish pre | string split0)\n").unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.install().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(contents.contains("q init"), "Amazon Q fish hook must stay: {contents}");
        assert!(
            contents.contains("ftab init"),
            "Fastab fish hook must be added: {contents}"
        );
    }

    #[tokio::test]
    async fn test_fish_script_install_preserves_fig_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(&path, "eval (fig init fish pre | string split0)\n").unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.install().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(contents.contains("fig init"), "Fig fish hook must stay: {contents}");
        assert!(
            contents.contains("ftab init"),
            "Fastab fish hook must be added: {contents}"
        );
    }

    #[tokio::test]
    async fn test_fish_uninstall_keeps_fig_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(
            &path,
            "# Fig pre block\neval (fig init fish pre | string split0)\ntest -x ~/.local/bin/ftab; and eval (~/.local/bin/ftab init fish pre --rcfile 00_fig_pre | string split0)\n",
        )
        .unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.uninstall().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(
            contents.contains("fig init"),
            "uninstall must leave Fig fish hook: {contents}"
        );
        assert!(
            contents.contains("Fig pre block"),
            "uninstall must leave Fig comment: {contents}"
        );
        assert!(
            !contents.contains("ftab init"),
            "uninstall must drop Fastab hook line: {contents}"
        );
    }

    #[tokio::test]
    async fn test_fish_uninstall_keeps_comment_that_mentions_fastab() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(
            &path,
            "# Easy Complete can sit beside Fastab\neval (ec init fish pre | string split0)\ntest -x ~/.local/bin/ftab; and eval (~/.local/bin/ftab init fish pre --rcfile 00_fig_pre | string split0)\n",
        )
        .unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.uninstall().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(
            contents.contains("ec init"),
            "uninstall must leave sibling fish hook: {contents}"
        );
        assert!(
            contents.contains("beside Fastab"),
            "a sibling comment that mentions Fastab must stay: {contents}"
        );
        assert!(
            !contents.contains("ftab init"),
            "uninstall must drop Fastab hook line: {contents}"
        );
    }

    #[tokio::test]
    async fn test_fish_migrate_does_not_truncate_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00_fig_pre.fish");
        std::fs::write(
            &path,
            "# Easy Complete pre block\neval (ec init fish pre | string split0)\n",
        )
        .unwrap();
        let integration = ShellScriptShellIntegration {
            shell: Shell::Fish,
            when: When::Pre,
            path,
        };
        integration.migrate().await.unwrap();
        let contents = std::fs::read_to_string(integration.path()).unwrap();
        assert!(
            contents.contains("ec init"),
            "desktop migrate must not wipe sibling fish: {contents}"
        );
        assert!(
            contents.contains("ftab init"),
            "desktop migrate must still add Fastab: {contents}"
        );
    }

    #[test]
    fn test_pre_matches_when_easy_complete_is_first() {
        let integration = zshrc_integration();
        let home = directories::home_dir().unwrap();
        let data = directories::fig_data_dir().unwrap();
        let rel = data.strip_prefix(&home).unwrap().display();
        let sibling = previous_product_data_dir().unwrap();
        let sibling_rel = sibling.strip_prefix(&home).unwrap().display();
        let doc = format!(
            "[[ -f \"${{HOME}}/{sibling_rel}/shell/zshrc.pre.zsh\" ]] && builtin source \"${{HOME}}/{sibling_rel}/shell/zshrc.pre.zsh\"\n[[ -f \"${{HOME}}/{rel}/shell/zshrc.pre.zsh\" ]] && builtin source \"${{HOME}}/{rel}/shell/zshrc.pre.zsh\"\n"
        );
        integration
            .matches_text(&doc, When::Pre)
            .expect("Easy Complete first must not fail Fastab pre position");
    }

    #[test]
    fn test_post_matches_when_easy_complete_is_last() {
        let home = directories::home_dir().unwrap();
        let data = directories::fig_data_dir().unwrap();
        let rel = data.strip_prefix(&home).unwrap().display();
        let sibling = previous_product_data_dir().unwrap();
        let sibling_rel = sibling.strip_prefix(&home).unwrap().display();
        let doc = format!(
            "[[ -f \"${{HOME}}/{rel}/shell/zshrc.post.zsh\" ]] && builtin source \"${{HOME}}/{rel}/shell/zshrc.post.zsh\"\n[[ -f \"${{HOME}}/{sibling_rel}/shell/zshrc.post.zsh\" ]] && builtin source \"${{HOME}}/{sibling_rel}/shell/zshrc.post.zsh\"\n"
        );
        let integration = DotfileShellIntegration {
            shell: Shell::Zsh,
            pre: false,
            post: true,
            dotfile_directory: home,
            dotfile_name: ".zshrc",
        };
        integration
            .matches_text(&doc, When::Post)
            .expect("Easy Complete last must not fail Fastab post position");
    }

    #[tokio::test]
    async fn test_install_inner_keeps_easy_complete_pre_first() {
        let dir = tempfile::tempdir().unwrap();
        let integration = DotfileShellIntegration {
            shell: Shell::Zsh,
            pre: true,
            post: false,
            dotfile_directory: dir.path().to_path_buf(),
            dotfile_name: ".zshrc",
        };
        let sibling = previous_product_data_dir().unwrap();
        let home = home_dir().unwrap();
        let sibling_rel = sibling.strip_prefix(&home).unwrap().display();
        std::fs::write(
            dir.path().join(".zshrc"),
            format!(
                "# Easy Complete pre block. Keep at the top of this file.\n[[ -f \"${{HOME}}/{sibling_rel}/shell/zshrc.pre.zsh\" ]] && builtin source \"${{HOME}}/{sibling_rel}/shell/zshrc.pre.zsh\"\nexport KEEP=1\n"
            ),
        )
        .unwrap();
        integration.install_inner().await.unwrap();
        let contents = std::fs::read_to_string(dir.path().join(".zshrc")).unwrap();
        let ec_pos = contents.find("easy-complete").expect("Easy Complete pre must stay");
        let ft_pos = contents.find("fastab/shell").expect("Fastab pre must be added");
        assert!(ec_pos < ft_pos, "Easy Complete pre must stay above Fastab: {contents}");
        assert!(contents.contains("export KEEP=1"), "user lines must stay: {contents}");
    }

    #[tokio::test]
    async fn test_install_inner_keeps_codewhisperer_pre() {
        let dir = tempfile::tempdir().unwrap();
        let integration = DotfileShellIntegration {
            shell: Shell::Zsh,
            pre: true,
            post: false,
            dotfile_directory: dir.path().to_path_buf(),
            dotfile_name: ".zshrc",
        };
        let q_dir = old_fig_data_dir().unwrap();
        let home = home_dir().unwrap();
        let q_rel = q_dir.strip_prefix(&home).unwrap().display();
        std::fs::write(
            dir.path().join(".zshrc"),
            format!(
                "# CodeWhisperer pre block. Keep at the top of this file.\n[[ -f \"${{HOME}}/{q_rel}/shell/zshrc.pre.zsh\" ]] && builtin source \"${{HOME}}/{q_rel}/shell/zshrc.pre.zsh\"\nexport KEEP=1\n"
            ),
        )
        .unwrap();
        integration.install_inner().await.unwrap();
        let contents = std::fs::read_to_string(dir.path().join(".zshrc")).unwrap();
        assert!(
            contents.contains("CodeWhisperer"),
            "Amazon Q comment must stay: {contents}"
        );
        assert!(
            contents.contains(&format!("{q_rel}/shell")),
            "Amazon Q source must stay: {contents}"
        );
        let q_pos = contents.find("CodeWhisperer").expect("Amazon Q pre must stay");
        let ft_pos = contents.find("fastab/shell").expect("Fastab pre must be added");
        assert!(q_pos < ft_pos, "Amazon Q pre must stay above Fastab: {contents}");
        assert!(contents.contains("export KEEP=1"), "user lines must stay: {contents}");
    }

    #[tokio::test]
    async fn test_uninstall_leaves_fig_please_make_sure_block() {
        let dir = tempfile::tempdir().unwrap();
        let integration = DotfileShellIntegration {
            shell: Shell::Zsh,
            pre: true,
            post: false,
            dotfile_directory: dir.path().to_path_buf(),
            dotfile_name: ".zshrc",
        };
        std::fs::write(
            dir.path().join(".zshrc"),
            "# Fig pre block. Please make sure this block is at the top of this file.\n[ -s ~/.fig/shell/pre.sh ] && source ~/.fig/shell/pre.sh\nexport KEEP=1\n",
        )
        .unwrap();
        integration.uninstall().await.unwrap();
        let contents = std::fs::read_to_string(dir.path().join(".zshrc")).unwrap();
        assert!(
            contents.contains("Please make sure this block"),
            "Fig comment must stay: {contents}"
        );
        assert!(
            contents.contains("~/.fig/shell/pre.sh"),
            "Fig source must stay: {contents}"
        );
        assert!(contents.contains("export KEEP=1"), "user lines must stay: {contents}");
    }

    #[test]
    fn test_nu_scripts_use_fastab_pty() {
        let pre = include_str!("scripts/pre.nu");
        assert!(
            pre.contains("{{PTY_BINARY_NAME}}"),
            "nu pre must launch Fastab's PTY: {pre}"
        );
        assert!(!pre.contains(".fig/bin"), "nu pre must not exec Fig's PTY: {pre}");
        assert!(!pre.contains("which figterm"), "nu pre must not look up figterm: {pre}");
        assert!(
            pre.contains("{{PTY_BINARY_PATH}}"),
            "nu pre needs the bundled fallback: {pre}"
        );
        assert!(
            pre.contains("Q_TERM_PATH"),
            "nu pre must retain explicit PTY overrides: {pre}"
        );
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Nu] {
            let pre = shell.get_fig_integration_source(&When::Pre);
            assert!(!pre.contains("{{PTY_BINARY_PATH}}"), "PTY path must be expanded: {pre}");
            assert!(
                !pre.contains("command -v fterm"),
                "PTY execution must not search PATH: {pre}"
            );
            assert!(
                !pre.contains("which fterm"),
                "PTY execution must not search PATH: {pre}"
            );
        }
        let post = include_str!("scripts/post.nu");
        assert!(
            post.contains("which {{CLI_BINARY_NAME}}"),
            "nu post must look up Fastab's CLI: {post}"
        );
        assert!(!post.contains("which fig "), "nu post must not look up fig: {post}");
    }

    #[test]
    fn test_fish_pre_q_parent_matches_bash() {
        let fish = include_str!("scripts/pre.fish");
        assert!(
            fish.contains("Q_SET_PARENT_CHECK"),
            "fish pre must use the same parent-guard as bash"
        );
        assert!(
            fish.contains("test -z \"$Q_PARENT\""),
            "fish pre must copy Q_SET_PARENT only when Q_PARENT is empty: {fish}"
        );
        assert!(
            fish.contains("test -n \"$Q_SET_PARENT\""),
            "fish pre must require Q_SET_PARENT: {fish}"
        );
    }

    #[test]
    fn test_previous_product_regex_leaves_fastab_blocks() {
        let integration = zshrc_integration();
        let data_dir = directories::fig_data_dir().unwrap();
        let dir = data_dir.strip_prefix(home_dir().unwrap()).unwrap().display();
        let source = format!(
            r#"[[ -f "${{HOME}}/{dir}/shell/zshrc.pre.zsh" ]] && builtin source "${{HOME}}/{dir}/shell/zshrc.pre.zsh""#
        );
        let doc = format!("# Fastab pre block. Keep at the top of this file.\n{source}\n");

        assert!(
            !integration.leftover_regexes(When::Pre).unwrap().is_match(&doc),
            "current Fastab blocks must not look like a leftover install"
        );
        integration
            .matches_text(&doc, When::Pre)
            .expect("a Fastab pre block at the top of the file is installed");
    }

    #[test]
    fn test_split_shebang() {
        let shebang = "#!/usr/bin/env sh";
        let contents = "echo hello world";
        let with_shebang = format!("{}\n{}", shebang, contents);
        let with_shebang_no_lf = format!("{}{}", shebang, contents);
        let without_shebang = contents;
        assert_eq!(
            (format!("{shebang}\n").as_str(), contents),
            split_shebang(&with_shebang),
            "split with shebang and linefeed"
        );
        assert_eq!(
            ("", format!("{shebang}{contents}").as_str()),
            split_shebang(&with_shebang_no_lf),
            "split with shebang and no linefeed"
        );
        assert_eq!(("", contents), split_shebang(without_shebang), "split with no shebang");
    }

    #[test]
    fn test_strip_trailing_otty_marker_block() {
        let body = indoc::indoc! {r#"
            export PATH="$HOME/.local/bin:$PATH"
            [[ -f "${HOME}/Library/Application Support/fastab/shell/bashrc.post.bash" ]] && builtin source "${HOME}/Library/Application Support/fastab/shell/bashrc.post.bash"
        "#};
        let otty = indoc::indoc! {r#"

            # >>> otty shell integration >>>
            # Added by Otty — toggle in Settings > Shell > Shell Integration.
            # Inert unless launched by Otty (it sets $OTTY_SHELL_INTEGRATION).
            if [ -n "$OTTY_SHELL_INTEGRATION" ] && [ -r "$OTTY_SHELL_INTEGRATION/otty-integration.bash" ]; then
              . "$OTTY_SHELL_INTEGRATION/otty-integration.bash"
            fi
            # <<< otty shell integration <<<
        "#};
        let full = format!("{body}{otty}");
        let stripped = strip_trailing_foreign_integrations(&full);
        assert_eq!(stripped, body.trim_end());

        let (split_body, trailing) = split_trailing_foreign_integrations(&full);
        assert_eq!(split_body.trim_end(), body.trim_end());
        assert!(trailing.contains(">>> otty shell integration >>>"));
        assert!(trailing.contains("<<< otty shell integration <<<"));
    }

    #[test]
    fn test_strip_trailing_otty_comment_stripped_form() {
        // Mimics DotfileShellIntegration::is_installed after comment/blank-line filtering.
        let filtered = indoc::indoc! {r#"
            [[ -f "${HOME}/Library/Application Support/fastab/shell/bashrc.post.bash" ]] && builtin source "${HOME}/Library/Application Support/fastab/shell/bashrc.post.bash"
            if [ -n "$OTTY_SHELL_INTEGRATION" ] && [ -r "$OTTY_SHELL_INTEGRATION/otty-integration.bash" ]; then
              . "$OTTY_SHELL_INTEGRATION/otty-integration.bash"
            fi
        "#};
        let stripped = strip_trailing_foreign_integrations(filtered);
        assert!(
            stripped.ends_with("bashrc.post.bash\" ]] && builtin source \"${HOME}/Library/Application Support/fastab/shell/bashrc.post.bash\""),
            "post source should remain: {stripped}"
        );
        assert!(
            !stripped.contains("OTTY_SHELL_INTEGRATION"),
            "Otty trailer should be ignored for position checks: {stripped}"
        );
    }

    #[test]
    fn test_post_matches_with_otty_trailer() {
        let home = directories::home_dir().unwrap();
        let data = directories::fig_data_dir().unwrap();
        let rel = data.strip_prefix(&home).unwrap().display();
        let post_line = format!(
            "[[ -f \"${{HOME}}/{rel}/shell/bashrc.post.bash\" ]] && builtin source \"${{HOME}}/{rel}/shell/bashrc.post.bash\""
        );
        let filtered = format!(
            "{post_line}\nif [ -n \"$OTTY_SHELL_INTEGRATION\" ] && [ -r \"$OTTY_SHELL_INTEGRATION/otty-integration.bash\" ]; then\n  . \"$OTTY_SHELL_INTEGRATION/otty-integration.bash\"\nfi\n"
        );

        let integration = DotfileShellIntegration {
            shell: Shell::Bash,
            pre: false,
            post: true,
            dotfile_directory: home,
            dotfile_name: ".bashrc",
        };
        integration
            .matches_text(&filtered, When::Post)
            .expect("Otty trailer must not fail post installation status");
    }

    const OTTY_1_6_BASHRC_BLOCK: &str = indoc::indoc! {r#"
        # >>> otty shell integration >>>
        # Added by Otty — toggle in Settings > Shell > Shell Integration.
        # Inert unless launched by Otty (it sets $OTTY_SHELL_INTEGRATION).
        if [ -n "$BASH_VERSION" ] && [ -n "$OTTY_SHELL_INTEGRATION" ] && [ -r "$OTTY_SHELL_INTEGRATION/otty-integration.bash" ]; then
          . "$OTTY_SHELL_INTEGRATION/otty-integration.bash"
        fi
        # <<< otty shell integration <<<
    "#};

    const OTTY_1_6_BASH_PROFILE_SHIM: &str = indoc::indoc! {r#"
        # >>> otty bash-profile shim >>>
        # Otty: login bash reads the profile, not ~/.bashrc — pull it in so
        # the shell-integration block in ~/.bashrc is reached (e.g. in tmux).
        if [ -n "$BASH_VERSION" ] && [ -f "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi
        # <<< otty bash-profile shim >>>
    "#};

    fn bash_post_integration(dotfile_name: &'static str) -> (DotfileShellIntegration, String) {
        let home = directories::home_dir().unwrap();
        let integration = DotfileShellIntegration {
            shell: Shell::Bash,
            pre: false,
            post: true,
            dotfile_directory: home,
            dotfile_name,
        };
        let post = format!(
            "{}\n{}\n",
            integration.description(When::Post),
            integration.source_text(When::Post).unwrap()
        );
        (integration, post)
    }

    /// The same filtering `is_installed` applies before `matches_text`.
    fn filter_like_is_installed(contents: &str) -> String {
        Regex::new(r"(?m)^\s*(#.*)?\n")
            .unwrap()
            .replace_all(contents, "")
            .trim()
            .to_owned()
    }

    #[test]
    fn test_post_matches_with_otty_1_6_trailers() {
        let cases = [
            (".bashrc", vec![OTTY_1_6_BASHRC_BLOCK]),
            (".bash_profile", vec![OTTY_1_6_BASH_PROFILE_SHIM]),
            (".bash_profile", vec![OTTY_1_6_BASH_PROFILE_SHIM, OTTY_1_6_BASHRC_BLOCK]),
        ];
        for (dotfile, trailers) in cases {
            let (integration, post) = bash_post_integration(dotfile);
            let trailing: String = trailers.iter().map(|t| format!("\n{t}")).collect();
            let contents = format!(". \"$HOME/.cargo/env\"\n{post}{trailing}");

            integration
                .matches_text(&filter_like_is_installed(&contents), When::Post)
                .unwrap_or_else(|err| panic!("{dotfile} with {trailers:?}: {err:?}"));

            let (body, split_trailing) = split_trailing_foreign_integrations(&contents);
            assert!(body.trim_end().ends_with(post.trim_end()), "{dotfile}: {body}");
            assert_eq!(split_trailing.trim(), trailing.trim(), "{dotfile}");
        }
    }

    #[test]
    fn test_post_repair_lands_above_otty_1_6_block() {
        let (integration, post) = bash_post_integration(".bashrc");
        // Settings repair strips the post block, then reinserts it above foreign trailers.
        let without_post = format!(". \"$HOME/.cargo/env\"\n\n{OTTY_1_6_BASHRC_BLOCK}");
        let (body, trailing) = split_trailing_foreign_integrations(&without_post);
        let repaired = format!("{}\n{post}{trailing}", body.trim_end());

        assert!(
            repaired.ends_with(OTTY_1_6_BASHRC_BLOCK),
            "Otty keeps the end: {repaired}"
        );
        integration
            .matches_text(&filter_like_is_installed(&repaired), When::Post)
            .expect("repaired .bashrc must pass the status check");
    }

    #[test]
    fn test_unknown_otty_block_is_not_a_trailer() {
        let contents =
            "export A=1\n\n# >>> otty path >>>\nexport PATH=\"$HOME/.otty/bin:$PATH\"\n# <<< otty path <<<\n";
        let (body, trailing) = split_trailing_foreign_integrations(contents);
        assert_eq!(
            body, contents,
            "install must append post below blocks status cannot peel"
        );
        assert!(trailing.is_empty());
    }

    #[test]
    fn test_mid_file_otty_shim_does_not_hide_a_misplaced_post() {
        let (integration, post) = bash_post_integration(".bash_profile");
        let contents = format!("{post}\n{OTTY_1_6_BASH_PROFILE_SHIM}\nexport AFTER=1\n");
        assert!(matches!(
            integration.matches_text(&filter_like_is_installed(&contents), When::Post),
            Err(Error::ImproperInstallation(_))
        ));
    }

    #[cfg(target_os = "linux")]
    fn all_dotfile_shell_integrations() -> Vec<ShellScriptShellIntegration> {
        Shell::all()
            .iter()
            .flat_map(|shell| shell.get_script_integrations().unwrap())
            .collect()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dotfile_shell_integrations_snapshot() {
        for integration in all_dotfile_shell_integrations() {
            let integration_name = format!(
                "{} {}",
                integration.describe(),
                integration.path.file_name().unwrap().to_str().unwrap()
            )
            .replace(' ', "_");
            insta::assert_snapshot!(integration_name, integration.get_contents());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dotfile_shell_integrations_shellcheck() {
        for integration in all_dotfile_shell_integrations() {
            run_shellcheck(integration.get_contents());
        }
    }
}
