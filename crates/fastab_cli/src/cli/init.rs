use std::borrow::Cow;
use std::fmt::Display;
use std::io::{Write, stdout};
use std::path::Path;
use std::process::ExitCode;
use std::sync::LazyLock;

use clap::Args;
use crossterm::style::Stylize;
use eyre::Result;
use fastab_integrations::shell::{ShellExt, When, post_handoff_variable};
use fastab_os_shim::{Context, Env};
use fastab_util::env_var::Q_SHELL;
use fastab_util::{CLI_BINARY_NAME, PRODUCT_NAME, Shell, Terminal, get_parent_process_exe};
use indoc::formatdoc;

use super::internal::should_figterm_launch::should_figterm_launch_exit_status;
use crate::util::app_path_from_bundle_id;
use crate::util::desktop::suppress_without_desktop_app;

const SHELL_INTEGRATIONS_ENABLED_STATE_KEY: &str = "shell-integrations.enabled";

static IS_SNAPSHOT_TEST: LazyLock<bool> = LazyLock::new(|| fastab_os_shim::Env::new().q_init_snapshot_test());

#[derive(Debug, Args, PartialEq, Eq)]
pub struct InitArgs {
    /// The shell to generate the dotfiles for
    #[arg(value_enum)]
    shell: Shell,
    /// When to generate the dotfiles for
    #[arg(value_enum)]
    when: When,
    #[arg(long)]
    rcfile: Option<String>,
}

impl InitArgs {
    pub async fn execute(&self) -> Result<ExitCode> {
        let InitArgs { shell, when, rcfile } = self;
        match shell_init(shell, when, rcfile).await {
            Ok(source) => writeln!(stdout(), "{source}"),
            Err(err) => writeln!(stdout(), "# Could not load source: {err}"),
        }
        .ok();
        Ok(ExitCode::SUCCESS)
    }
}

#[derive(PartialEq, Eq)]
enum GuardAssignment {
    BeforeSourcing,
    AfterSourcing,
}

/// Escapes `value` for a double-quoted string in `shell`. Values are paths
/// (`Q_SHELL`, the `qterm.path` setting); a `$`, backtick or quote in one must
/// stay literal instead of being expanded or ending the string early.
fn escape_double_quoted(shell: &Shell, value: &str) -> String {
    let special: &[char] = match shell {
        Shell::Bash | Shell::Zsh => &['\\', '"', '$', '`'],
        Shell::Fish => &['\\', '"', '$'],
        Shell::Nu => &['\\', '"'],
    };
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if special.contains(&ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn assign_shell_variable(shell: &Shell, name: impl Display, value: impl Display, exported: bool) -> String {
    let value = escape_double_quoted(shell, &value.to_string());
    match (shell, exported) {
        (Shell::Bash | Shell::Zsh, false) => format!("{name}=\"{value}\""),
        (Shell::Bash | Shell::Zsh, true) => format!("export {name}=\"{value}\""),
        (Shell::Fish, false) => format!("set -g {name} \"{value}\""),
        (Shell::Fish, true) => format!("set -gx {name} \"{value}\""),
        // `let-env` was removed from Nushell (0.83).
        (Shell::Nu, _) => format!("$env.{name} = \"{value}\""),
    }
}

fn guard_source(
    shell: &Shell,
    export: bool,
    guard_var: impl Display,
    assignment: GuardAssignment,
    source: impl Into<Cow<'static, str>>,
) -> String {
    let mut output: Vec<Cow<'static, str>> = Vec::with_capacity(4);
    output.push(match shell {
        Shell::Bash | Shell::Zsh => format!("if [ -z \"${{{guard_var}}}\" ]; then").into(),
        Shell::Fish => format!("if test -z \"${guard_var}\"").into(),
        // Same sense as the other shells: run only while the guard is unset.
        Shell::Nu => format!("if '{guard_var}' not-in $env {{").into(),
    });

    let shell_var = assign_shell_variable(shell, guard_var, "1", export);
    match assignment {
        GuardAssignment::BeforeSourcing => {
            // If script may trigger rc file to be rerun, guard assignment must happen first to avoid recursion
            output.push(format!("  {shell_var}").into());
            for line in source.into().lines() {
                output.push(format!("  {line}").into());
            }
        },
        GuardAssignment::AfterSourcing => {
            for line in source.into().lines() {
                output.push(format!("  {line}").into());
            }
            output.push(format!("  {shell_var}").into());
        },
    }

    output.push(
        match shell {
            Shell::Bash | Shell::Zsh => "fi\n",
            Shell::Fish => "end\n",
            Shell::Nu => "}",
        }
        .into(),
    );

    output.join("\n")
}

async fn shell_init(shell: &Shell, when: &When, rcfile: &Option<String>) -> Result<String> {
    // Do not print any shell integrations for `.profile` as it can cause issues on launch
    if std::env::consts::OS == "linux" && matches!(rcfile.as_deref(), Some("profile")) {
        return Ok("".into());
    }

    if !fastab_settings::state::get_bool_or(SHELL_INTEGRATIONS_ENABLED_STATE_KEY, true) {
        return Ok(shell_integrations_disabled_code(*shell));
    }

    // When the desktop app is not running, skip pre/post hooks entirely so other
    // terminal integrations (VS Code Terminal Suggest, Otty, etc.) keep working.
    if !*IS_SNAPSHOT_TEST && suppress_without_desktop_app(&Env::new()) {
        let mut source = format!("# {PRODUCT_NAME} desktop app is not running; skipping shell integration\n");
        if when == &When::Pre && hands_over_post(shell) {
            // The post run would only reach the same verdict.
            source.push_str(&format!("{}=''\n", post_handoff_variable(rcfile.as_deref())));
        }
        return Ok(source);
    }

    let mut to_source = Vec::new();
    // Assignments both runs make; the post script handed over by pre repeats them.
    let mut shared = Vec::new();

    if let Some(parent_process) = get_parent_process_exe() {
        shared.push(assign_shell_variable(
            shell,
            Q_SHELL,
            if *IS_SNAPSHOT_TEST {
                Path::new("/bin/zsh").display()
            } else {
                parent_process.display()
            },
            false,
        ));
    };
    to_source.extend(shared.iter().cloned());

    if when == &When::Pre {
        let status = if *IS_SNAPSHOT_TEST {
            0
        } else {
            should_figterm_launch_exit_status(&Context::new(), true)
        };
        to_source.push(assign_shell_variable(shell, "SHOULD_QTERM_LAUNCH", status, false));
    }

    let is_jetbrains_terminal = Terminal::is_jetbrains_terminal();

    if when == &When::Pre && shell == &Shell::Bash && is_jetbrains_terminal {
        // JediTerm does not launch as a 'true' login shell, so our normal "shopt -q login_shell" check does
        // not work. Thus, Q_IS_LOGIN_SHELL will be incorrect. We must manually set it so the
        // user's bash_profile is sourced. https://github.com/JetBrains/intellij-community/blob/master/plugins/terminal/resources/jediterm-bash.in
        to_source.push("Q_IS_LOGIN_SHELL=1".into());
    }

    if let Some(path) = fastab_settings::settings::get_string_opt("qterm.path") {
        let assignment = assign_shell_variable(shell, "Q_TERM_PATH", path, false);
        to_source.push(assignment.clone());
        shared.push(assignment);
    }

    let shell_integration_source = shell.get_fig_integration_source(when);
    to_source.push(shell_integration_source);

    if when == &When::Pre && is_jetbrains_terminal {
        // Manually call JetBrains shell integration after exec-ing to figterm.
        // This may recursively call out to bashrc/zshrc so make sure to assign guard variable first.

        let get_jetbrains_source = if let Some(bundle_id) = std::env::var_os("__CFBundleIdentifier") {
            if let Some(bundle) = app_path_from_bundle_id(bundle_id) {
                // The source for JetBrains shell integrations can be found here.
                // https://github.com/JetBrains/intellij-community/tree/master/plugins/terminal/resources

                // We source both the old and new location of these integrations.
                // In theory, they shouldn't both exist since they come with the app bundle itself.
                // As of writing, the bash path change isn't live, but we source it anyway.
                match shell {
                    Shell::Bash => Some(formatdoc! {"
                        [ -f '{bundle}/Contents/plugins/terminal/jediterm-bash.in' ] && source '{bundle}/Contents/plugins/terminal/jediterm-bash.in'
                        [ -f '{bundle}/Contents/plugins/terminal/bash/jediterm-bash.in' ] && source '{bundle}/Contents/plugins/terminal/bash/jediterm-bash.in'
                    "}),
                    Shell::Zsh => Some(formatdoc! {"
                        [ -f '{bundle}/Contents/plugins/terminal/.zshenv' ] && source '{bundle}/Contents/plugins/terminal/.zshenv'
                        [ -f '{bundle}/Contents/plugins/terminal/zsh/.zshenv' ] && source '{bundle}/Contents/plugins/terminal/zsh/.zshenv'
                    "}),
                    Shell::Fish => Some(formatdoc! {"
                        [ -f '{bundle}/Contents/plugins/terminal/fish/config.fish' ] && source '{bundle}/Contents/plugins/terminal/fish/config.fish'
                        [ -f '{bundle}/Contents/plugins/terminal/fish/init.fish' ] && source '{bundle}/Contents/plugins/terminal/fish/init.fish'
                    "}),
                    Shell::Nu => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(source) = get_jetbrains_source {
            to_source.push(guard_source(
                shell,
                false,
                "Q_JETBRAINS_SHELL_INTEGRATION",
                GuardAssignment::BeforeSourcing,
                source,
            ));
        }
    }

    #[cfg(target_os = "macos")]
    if when == &When::Post
        && let Some(prompt) = input_method_prompt()
    {
        let _ = fastab_settings::state::set_value(&prompt.state_key, prompt.count + 1);
        to_source.push(input_method_prompt_code(*shell, &prompt.terminal));
    }

    // Hand the post script to the post shim, which then runs nothing. Not
    // when post owes an input method prompt: only the post run may count one,
    // and a pre run can belong to the shell that execs into fterm and never
    // reaches post.
    if when == &When::Pre && hands_over_post(shell) && (*IS_SNAPSHOT_TEST || !prompts_for_input_method()) {
        shared.push(shell.get_fig_integration_source(&When::Post));
        to_source.push(format!(
            "# shellcheck disable=SC1003,SC2016,SC2034\n{}={}",
            post_handoff_variable(rcfile.as_deref()),
            single_quoted(&shared.join("\n"))
        ));
    }

    Ok(to_source.join("\n"))
}

/// Only the zsh and bash post shims read the handed-over script.
fn hands_over_post(shell: &Shell) -> bool {
    matches!(shell, Shell::Bash | Shell::Zsh)
}

/// `'…'`: a POSIX single-quoted word, closing and reopening the quotes around
/// each `'` in `value`.
fn single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(target_os = "macos")]
struct InputMethodPrompt {
    state_key: String,
    count: i64,
    terminal: Terminal,
}

/// The input method prompt the post script shows at most twice per terminal.
#[cfg(target_os = "macos")]
fn input_method_prompt() -> Option<InputMethodPrompt> {
    if fastab_integrations::input_method::InputMethod::default()
        .is_enabled()
        .unwrap_or(false)
    {
        return None;
    }
    let terminal = Terminal::parent_terminal(&Context::new())?;
    let state_key = format!("prompt.input-method.{}.count", terminal.internal_id());
    let count = fastab_settings::state::get_int_or(&state_key, 0);
    let running_version = if terminal == Terminal::Otty {
        terminal
            .to_bundle_id()
            .and_then(|bundle| macos_utils::applications::unique_running_application_version(&bundle))
    } else {
        None
    };
    (terminal.supports_macos_input_method()
        && !terminal.prefers_macos_accessibility(running_version.as_deref())
        && count < 2)
        .then_some(InputMethodPrompt {
            state_key,
            count,
            terminal,
        })
}

fn prompts_for_input_method() -> bool {
    #[cfg(target_os = "macos")]
    {
        input_method_prompt().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn shell_integrations_disabled_code(shell: Shell) -> String {
    guard_source(
        &shell,
        false,
        "Q_SHELL_INTEGRATION_DISABLED",
        GuardAssignment::AfterSourcing,
        format!(
            "printf '{PRODUCT_NAME} shell integration is disabled.\\nRe-enable by running: {}\\n'",
            format!("{CLI_BINARY_NAME} _ local-state -d {SHELL_INTEGRATIONS_ENABLED_STATE_KEY}").magenta()
        ),
    )
}

#[allow(dead_code)]
fn input_method_prompt_code(shell: Shell, terminal: &Terminal) -> String {
    guard_source(
        &shell,
        false,
        "Q_INPUT_METHOD_PROMPT",
        GuardAssignment::AfterSourcing,
        format!(
            "printf '\\n🚀 {PRODUCT_NAME} supports {terminal}!\\n\\nEnable integrations with {terminal} by \
             running:\\n  {}\\n\\n'\n",
            format!("{CLI_BINARY_NAME} integrations install input-method").magenta()
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn assigned_values_stay_literal_inside_double_quotes() {
        let path = r#"/opt/a"b$c`d\e"#;
        assert_eq!(
            assign_shell_variable(&Shell::Zsh, "Q_TERM_PATH", path, false),
            r#"Q_TERM_PATH="/opt/a\"b\$c\`d\\e""#
        );
        assert_eq!(
            assign_shell_variable(&Shell::Fish, "Q_TERM_PATH", path, true),
            r#"set -gx Q_TERM_PATH "/opt/a\"b\$c`d\\e""#
        );
        assert_eq!(
            assign_shell_variable(&Shell::Nu, "Q_TERM_PATH", path, false),
            r#"$env.Q_TERM_PATH = "/opt/a\"b$c`d\\e""#
        );
        assert_eq!(
            assign_shell_variable(&Shell::Bash, "Q_SHELL", "/bin/zsh", false),
            r#"Q_SHELL="/bin/zsh""#
        );
    }

    #[test]
    fn the_handed_over_post_script_survives_quoting_in_zsh_and_bash() {
        let script = "printf '%s\\n' \"it's $HOME\" 'done'\n";
        for shell in [Shell::Zsh, Shell::Bash] {
            let handoff = format!("x={}\nprintf %s \"$x\"", single_quoted(script));
            let output = Command::new(shell.as_str()).args(["-c", &handoff]).output().unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), script, "{shell:?}");
        }
    }

    #[test]
    fn nu_guard_runs_only_while_unset() {
        let output = guard_source(&Shell::Nu, false, "G", GuardAssignment::BeforeSourcing, "echo hi");
        assert!(
            output.starts_with("if 'G' not-in $env {\n  $env.G = \"1\"\n"),
            "{output}"
        );
    }

    fn run_shell_stdout(shell: &Shell, text: &str) -> String {
        let mut child = Command::new(shell.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.as_mut().unwrap();
        // Since these are all guarded we run the code twice to double check
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();

        let output = child.wait_with_output().unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn test_prompts() {
        for shell in Shell::all_test() {
            let shell_integrations_disabled_output = run_shell_stdout(&shell, &shell_integrations_disabled_code(shell));

            println!("=== shell_integrations_disabled_code {shell:?} ===");
            println!("{shell_integrations_disabled_output}");
            println!("===");

            assert_eq!(
                shell_integrations_disabled_output,
                format!(
                    "{PRODUCT_NAME} shell integration is disabled.\nRe-enable by running: {}\n",
                    format!("{CLI_BINARY_NAME} _ local-state -d {SHELL_INTEGRATIONS_ENABLED_STATE_KEY}").magenta()
                )
            );

            let terminal = Terminal::Iterm;
            let input_method_prompt_output = run_shell_stdout(&shell, &input_method_prompt_code(shell, &terminal));

            println!("=== input_method_prompt {shell:?} ===");
            println!("{input_method_prompt_output}");
            println!("===");

            assert_eq!(
                input_method_prompt_output,
                format!(
                    "\n🚀 {PRODUCT_NAME} supports {terminal}!\n\nEnable integrations with {terminal} by running:\n  {}\n\n",
                    format!("{CLI_BINARY_NAME} integrations install input-method").magenta()
                )
            );
        }
    }
}
