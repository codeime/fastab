use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Do not open Settings when starting
    #[arg(long)]
    pub no_dashboard: bool,
    /// Checks the `app.launchOnStartup` setting before launching
    #[arg(long)]
    pub is_startup: bool,
    /// Kill old FasTab instances
    #[arg(long)]
    pub kill_old: bool,
    /// Allow launching multiple FasTab instances
    #[arg(long)]
    pub allow_multiple: bool,
    /// Don't attempt to update right away
    #[arg(long)]
    pub ignore_immediate_update: bool,
    /// Unregister launch at login and exit (used by the uninstaller)
    #[arg(long, hide = true)]
    pub unregister_login_item: bool,
    /// Run only the Settings window, for the desktop app listening on this socket
    #[arg(long, hide = true, value_name = "SOCKET")]
    pub settings_window: Option<PathBuf>,
    /// Url to open
    pub url_link: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_app() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
