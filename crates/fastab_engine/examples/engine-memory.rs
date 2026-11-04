//! What completions leave in the process's heap, to compare builds of the engine.
//!
//!     cargo run --profile dist -p fastab_engine --example engine-memory -- bundle/specs-ir
//!
//! Engine only: no window, no GPUI, no AppKit, so a difference between two
//! builds is the engine's. Requests come from zsh with HOME on a scratch
//! directory holding a 10,000-command `.zsh_history` (empty with
//! `--no-history`), one keystroke at a time, with the engine retiring its caches
//! between bursts as it does after input ends. Prints `phase footprint in_use
//! allocated` in bytes per line; `scripts/engine-memory-ab.py` compares builds.
//! Generator rows run real commands in the current directory.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use fastab_engine::{CompleteRequest, EngineClient, EngineClientOptions, SessionId};

const GRACE: Duration = Duration::from_millis(100);
const BURSTS: usize = 6;
const HISTORY_COMMANDS: usize = 10_000;
const COMMANDS: &[&str] = &[
    "git checkout ",
    "git add ",
    "git status",
    "npm run ",
    "docker run -",
    "cargo build",
    "kubectl get po",
    "ls -l",
    "cd ",
];
const SESSION: SessionId = SessionId::new(1);

// The layouts these calls fill; only some fields are read.
#[allow(dead_code)]
#[repr(C)]
#[derive(Default)]
struct ZoneStatistics {
    blocks_in_use: u32,
    size_in_use: usize,
    max_size_in_use: usize,
    size_allocated: usize,
}

/// `rusage_info_v0`.
#[allow(dead_code)]
#[repr(C)]
#[derive(Default)]
struct ResourceUsage {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    package_idle_wakeups: u64,
    interrupt_wakeups: u64,
    page_ins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    process_start: u64,
    process_exit: u64,
}

unsafe extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut ResourceUsage) -> i32;
    fn malloc_zone_statistics(zone: *mut c_void, statistics: *mut ZoneStatistics);
    fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
}

fn sample(phase: &str) {
    let mut usage = ResourceUsage::default();
    let mut zones = ZoneStatistics::default();
    // SAFETY: both buffers are the layouts these calls fill, and a null zone
    // means every zone.
    unsafe {
        proc_pid_rusage(std::process::id() as i32, 0, &raw mut usage);
        malloc_zone_statistics(std::ptr::null_mut(), &raw mut zones);
    }
    println!(
        "{phase} {} {} {}",
        usage.phys_footprint, zones.size_in_use, zones.size_allocated
    );
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let specs = args
        .next()
        .map_or_else(|| PathBuf::from("bundle/specs-ir"), PathBuf::from);
    let with_history = !args.any(|arg| arg == "--no-history");
    ensure!(specs.join("index.json").is_file(), "no spec IR at {}", specs.display());
    let cwd = std::env::current_dir()?.display().to_string();
    let home = tempfile::tempdir()?;
    if with_history {
        write_zsh_history(home.path())?;
    }
    let environment = Arc::new(vec![("HOME".to_string(), home.path().display().to_string())]);
    sample("start");
    let client = EngineClient::spawn_with_options(specs, EngineClientOptions { spec_idle_grace: GRACE })?;
    let complete = |buffer: &str| -> anyhow::Result<()> {
        let request = CompleteRequest {
            buffer: buffer.into(),
            cwd: cwd.clone(),
            current_shell: Some("/bin/zsh".into()),
            environment_variables: Arc::clone(&environment),
            ..CompleteRequest::default()
        };
        futures::executor::block_on(client.complete_for_session(SESSION, request))
            .with_context(|| format!("complete {buffer:?}"))?;
        Ok(())
    };
    complete("fastab-engine-memory-unknown ")?;
    sample("built");
    for burst in 0..BURSTS {
        for command in COMMANDS {
            for end in 1..=command.len() {
                complete(&command[..end])?;
            }
        }
        if burst + 1 == BURSTS {
            sample("busy");
        }
        client.end_input(SESSION)?;
        std::thread::sleep(GRACE * 4);
    }
    sample("idle");
    // As the desktop's heap_trim does once the app goes quiet.
    // SAFETY: a null zone means every zone; no pointer is retained.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    std::thread::sleep(GRACE * 2);
    sample("relieved");
    Ok(())
}

/// An EXTENDED_HISTORY file with about as many distinct commands as a real one.
fn write_zsh_history(home: &Path) -> anyhow::Result<()> {
    let mut state: u32 = 1;
    let mut next = |bound: u32| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 16) % bound
    };
    let mut history = String::new();
    for i in 0..HISTORY_COMMANDS {
        let command = match next(8) {
            0 => "git status".to_string(),
            1 => format!("git checkout feature/branch-{}", next(400)),
            2 => format!("cargo test -p crate_{} -- --nocapture test_{}", next(30), next(300)),
            3 => format!("cd ~/src/project-{}/module-{}", next(60), next(40)),
            4 => format!("docker run -it --rm -v $PWD:/work image-{}:{} bash", next(40), next(20)),
            5 => format!("npm run build -- --filter=pkg-{}", next(200)),
            6 => format!("curl -s https://example.com/api/v{}/item/{}", next(4), next(800)),
            _ => format!("grep -rn token_{} src/dir_{}", next(500), next(50)),
        };
        writeln!(history, ": {}:0;{command}", 1_700_000_000 + i)?;
    }
    std::fs::write(home.join(".zsh_history"), history)?;
    Ok(())
}
