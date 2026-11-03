pub mod cli;
pub mod util;

use std::process::ExitCode;

use anstream::eprintln;
use clap::Parser;
use clap::error::{ContextKind, ErrorKind};
use crossterm::style::Stylize;
use eyre::Result;
use fastab_log::get_log_level_max;
use fastab_util::{CLI_BINARY_NAME, PRODUCT_NAME};
use tracing::metadata::LevelFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<ExitCode> {
    color_eyre::install()?;

    let mut args = std::env::args();
    let subcommand = args.nth(1);
    // Every shell start runs `init` (several times for a login shell) and
    // every prompt may run `pre-cmd`. They await nothing that needs a second
    // thread, so they skip the per-core worker pool, and they open SQLite
    // directly instead of starting a pool and its maintenance thread for one
    // or two reads, as fterm does.
    let shell_hot_path = match subcommand.as_deref() {
        Some("init") => true,
        Some("_" | "internal") => matches!(
            args.next().as_deref(),
            Some("pre-cmd" | "should-figterm-launch" | "get-shell")
        ),
        _ => false,
    };
    if shell_hot_path {
        fastab_settings::sqlite::prefer_direct_connections();
    }
    let multithread = !shell_hot_path
        && matches!(
            subcommand.as_deref(),
            Some("init" | "_" | "internal" | "completion" | "hook")
        );

    let parsed = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();

            let unknown_arg = matches!(err.kind(), ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand)
                && !err.context().any(|(context_kind, _)| {
                    matches!(
                        context_kind,
                        ContextKind::SuggestedSubcommand | ContextKind::SuggestedArg
                    )
                });

            if unknown_arg {
                eprintln!(
                    "\nThis command may be valid in newer versions of the {PRODUCT_NAME} CLI. Try running {} {}.",
                    CLI_BINARY_NAME.magenta(),
                    "update".magenta()
                );
            }

            return Ok(ExitCode::from(err.exit_code().try_into().unwrap_or(2)));
        },
    };

    let verbose = parsed.verbose > 0;

    let runtime = if multithread {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    }
    .enable_all()
    .build()?;

    let result = runtime.block_on(parsed.execute());

    match result {
        Ok(exit_code) => Ok(exit_code),
        Err(err) => {
            if verbose || get_log_level_max() > LevelFilter::INFO {
                eprintln!("{} {err:?}", "error:".bold().red());
            } else {
                eprintln!("{} {err}", "error:".bold().red());
            }
            Ok(ExitCode::FAILURE)
        },
    }
}
