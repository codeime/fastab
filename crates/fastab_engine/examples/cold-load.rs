//! The first completion after the engine retired its caches, against a warm one.
//!
//!     cargo run --profile dist -p fastab_engine --example cold-load -- bundle/specs-ir
//!
//! The desktop drops parsed specs, the typed-hook catalog and hook results 10 s
//! after input ends, so the first keystroke of most commands rebuilds them.
//! This replays that with a short grace and prints medians in ms, then splits
//! a cold spec load and a cold catalog load into their steps. Generator rows
//! run real commands in the current directory. Requests come from zsh with
//! HOME on a scratch directory, and the rounds run on an engine that loaded a
//! 10,000-command `.zsh_history` the way the desktop does.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use fastab_engine::{CompleteRequest, EngineClient, EngineClientOptions, Registry, SessionId, Spec};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

const GRACE: Duration = Duration::from_millis(100);
const ROUNDS: usize = 7;
const BUFFERS: &[&str] = &["git che", "git checkout ", "npm run ", "docker run -", "cargo b"];
const SPECS: &[&str] = &["git", "docker", "npm", "kubectl", "cargo"];
const SESSION: SessionId = SessionId::new(1);
const HISTORY_COMMANDS: usize = 10_000;

struct Probe {
    client: EngineClient,
    cwd: String,
    environment: Arc<Vec<(String, String)>>,
}

impl Probe {
    fn complete(&self, buffer: &str) -> anyhow::Result<Duration> {
        let request = CompleteRequest {
            buffer: buffer.into(),
            cwd: self.cwd.clone(),
            include_history: false,
            current_shell: Some("/bin/zsh".into()),
            environment_variables: Arc::clone(&self.environment),
            ..CompleteRequest::default()
        };
        let started = Instant::now();
        futures::executor::block_on(self.client.complete_for_session(SESSION, request))
            .with_context(|| format!("complete {buffer:?}"))?;
        Ok(started.elapsed())
    }
}

/// A new engine's first request; an unknown command loads no spec and runs
/// no hook, so it times the build and the history load alone.
fn first_request(specs: &Path, cwd: &str, home: &Path, label: &str) -> anyhow::Result<Probe> {
    let started = Instant::now();
    let client = EngineClient::spawn_with_options(specs.to_path_buf(), EngineClientOptions { spec_idle_grace: GRACE })?;
    let spawned = started.elapsed();
    let probe = Probe {
        client,
        cwd: cwd.to_string(),
        environment: Arc::new(vec![("HOME".to_string(), home.display().to_string())]),
    };
    let engine = probe.complete("fastab-cold-load-unknown ")?;
    let git = probe.complete("git ")?;
    println!(
        "first request, {label}: {:.1} = client spawn {:.1}, engine build {:.1}, then `git ` {:.1}",
        ms(started.elapsed()),
        ms(spawned),
        ms(engine),
        ms(git)
    );
    Ok(probe)
}

fn main() -> anyhow::Result<()> {
    let specs = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("bundle/specs-ir"), PathBuf::from);
    ensure!(specs.join("index.json").is_file(), "no spec IR at {}", specs.display());
    let cwd = std::env::current_dir()?.display().to_string();
    let empty_home = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let history_bytes = write_zsh_history(home.path())?;
    // The first engine pays for cold files, the second is the same build warm,
    // and the third differs from it only by the history it loads.
    first_request(&specs, &cwd, empty_home.path(), "cold, no history")?;
    first_request(&specs, &cwd, empty_home.path(), "warm, no history")?;
    let probe = first_request(
        &specs,
        &cwd,
        home.path(),
        &format!(
            "warm, {HISTORY_COMMANDS}-command zsh history ({} KB)",
            history_bytes / 1024
        ),
    )?;
    // What the first request used to spend on history: a login zsh, which
    // sources the profile but not /etc/zshrc, so it has no HISTFILE.
    let started = Instant::now();
    let old = std::process::Command::new("zsh")
        .args(["-lc", "fc -R; fc -ln -10000"])
        .current_dir("/")
        .output();
    println!(
        "former history source `zsh -lc 'fc -R; fc -ln -10000'`: {:.1} ({})",
        ms(started.elapsed()),
        old.map_or_else(
            |error| format!("failed: {error}"),
            |output| format!(
                "exit {:?}, {} lines",
                output.status.code(),
                output
                    .stdout
                    .split(|&byte| byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .count()
            )
        )
    );
    for buffer in BUFFERS {
        let mut idle = Vec::with_capacity(ROUNDS);
        let mut warm = Vec::with_capacity(ROUNDS);
        let mut typing = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            probe.client.end_input(SESSION)?;
            std::thread::sleep(GRACE * 4);
            idle.push(probe.complete(buffer)?);
            warm.push(probe.complete(buffer)?);
            // The next keystroke in the same token, as typing produces it.
            typing.push(probe.complete(&format!("{buffer}x"))?);
        }
        println!(
            "{buffer:?}: after idle {:.1}, warm {:.1}, next key {:.1}",
            ms(median(&mut idle)),
            ms(median(&mut warm)),
            ms(median(&mut typing))
        );
    }
    // What "after idle" rebuilt: nothing may stay cached past the grace.
    probe.client.end_input(SESSION)?;
    std::thread::sleep(GRACE * 4);
    let engine = futures::executor::block_on(probe.client.diagnostics())?
        .engine
        .context("no engine")?;
    println!(
        "after idle: {} cached spec files, hook catalog loaded: {}",
        engine.registry.cached_file_count, engine.hook_catalog.loaded
    );

    let started = Instant::now();
    Registry::load(&specs)?;
    println!(
        "registry open (index + digest of every IR file): {:.1}",
        ms(started.elapsed())
    );
    for line in SPECS
        .iter()
        .map(|name| spec_phases(&specs, name))
        .chain([catalog_phases(&specs)])
    {
        match line {
            Ok(line) => println!("{line}"),
            Err(error) => println!("phase split failed: {error:#}"),
        }
    }
    for argv in [
        &["git", "help", "-a"][..],
        &["git", "config", "--get-regexp", "^alias\\."][..],
    ] {
        let started = Instant::now();
        let _ = std::process::Command::new(argv[0]).args(&argv[1..]).output();
        println!("{}: {:.1}", argv.join(" "), ms(started.elapsed()));
    }
    Ok(())
}

/// The registry reads a spec, checks its digest, parses it, then interns
/// and shrinks the tree. The first three are replayed by hand; the rest is
/// what `get_arc` costs on top of them.
fn spec_phases(specs: &Path, name: &str) -> anyhow::Result<String> {
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(specs.join("index.json"))?)?;
    let file = index["files"][name].as_str().context("not in index.json")?;
    let (bytes, read) = timed(|| std::fs::read(specs.join(file)));
    let bytes = bytes?;
    let (_, hash) = timed(|| std::hint::black_box(Sha256::digest(&bytes)));
    let (spec, parse) = timed(|| serde_json::from_slice::<Spec>(&bytes));
    drop(spec?);
    let mut registry = Registry::load(specs)?;
    let (loaded, total) = timed(|| registry.get_arc(name));
    loaded.context("registry load")?;
    Ok(format!(
        "spec {name} ({} KB): read {:.1}, sha256 {:.1}, parse {:.1}, whole registry load {:.1}",
        bytes.len() / 1024,
        ms(read),
        ms(hash),
        ms(parse),
        ms(total)
    ))
}

/// The catalog parse boxes every descriptor; a borrowed scan is what an
/// offset index built over the same bytes would cost.
fn catalog_phases(specs: &Path) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct Owned {
        hooks: HashMap<String, Box<RawValue>>,
    }
    #[derive(serde::Deserialize)]
    struct Borrowed<'a> {
        #[serde(borrow)]
        hooks: HashMap<&'a str, &'a RawValue>,
    }
    let (bytes, read) = timed(|| std::fs::read(specs.join("typed-hooks.json")));
    let bytes = bytes?;
    let (_, hash) = timed(|| std::hint::black_box(Sha256::digest(&bytes)));
    let (owned, owned_parse) = timed(|| serde_json::from_slice::<Owned>(&bytes));
    let hooks = owned?.hooks.len();
    let (borrowed, borrowed_scan) = timed(|| serde_json::from_slice::<Borrowed<'_>>(&bytes).map(|b| b.hooks.len()));
    borrowed?;
    Ok(format!(
        "catalog ({} KB, {hooks} hooks): read {:.1}, sha256 {:.1}, boxed parse {:.1}, borrowed scan {:.1}",
        bytes.len() / 1024,
        ms(read),
        ms(hash),
        ms(owned_parse),
        ms(borrowed_scan)
    ))
}

/// An EXTENDED_HISTORY file shaped like a working developer's.
fn write_zsh_history(home: &Path) -> anyhow::Result<usize> {
    let mut history = String::new();
    for i in 0..HISTORY_COMMANDS {
        let command = match i % 6 {
            0 => format!("git checkout feature/branch-{}", i % 97),
            1 => "git status".to_string(),
            2 => format!("cargo test -p crate_{} -- --nocapture", i % 23),
            3 => format!("cd ~/src/project-{}", i % 41),
            4 => format!("docker run -it --rm -v $PWD:/work image-{}:latest bash", i % 13),
            _ => format!("npm run build -- --filter=pkg-{}", i % 17),
        };
        writeln!(history, ": {}:0;{command}", 1_700_000_000 + i)?;
    }
    std::fs::write(home.join(".zsh_history"), &history)?;
    Ok(history.len())
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = f();
    (value, started.elapsed())
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
