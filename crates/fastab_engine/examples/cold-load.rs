//! The first completion after the engine retired its caches, against a warm one.
//!
//!     cargo run --profile dist -p fastab_engine --example cold-load -- bundle/specs-ir
//!
//! The desktop drops parsed specs, the typed-hook catalog and hook results 10 s
//! after input ends, so the first keystroke of most commands rebuilds them.
//! This replays that with a short grace and prints medians in ms. Generator
//! rows run real commands in the current directory.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use fastab_engine::{CompleteRequest, EngineClient, EngineClientOptions, SessionId};

const GRACE: Duration = Duration::from_millis(100);
const ROUNDS: usize = 7;
const BUFFERS: &[&str] = &["git che", "git checkout ", "npm run ", "docker run -"];
const SESSION: SessionId = SessionId::new(1);

fn main() -> anyhow::Result<()> {
    let specs = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("bundle/specs-ir"), PathBuf::from);
    ensure!(specs.join("index.json").is_file(), "no spec IR at {}", specs.display());
    let cwd = std::env::current_dir()?.display().to_string();
    let started = Instant::now();
    let client = EngineClient::spawn_with_options(specs, EngineClientOptions { spec_idle_grace: GRACE })?;
    let complete = |buffer: &str| -> anyhow::Result<Duration> {
        let request = CompleteRequest {
            buffer: buffer.into(),
            cwd: cwd.clone(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let started = Instant::now();
        futures::executor::block_on(client.complete_for_session(SESSION, request))
            .with_context(|| format!("complete {buffer:?}"))?;
        Ok(started.elapsed())
    };
    complete("git ")?;
    println!("first request, engine start included: {:.1}", ms(started.elapsed()));
    for buffer in BUFFERS {
        let mut idle = Vec::with_capacity(ROUNDS);
        let mut warm = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            client.end_input(SESSION)?;
            std::thread::sleep(GRACE * 4);
            idle.push(complete(buffer)?);
            warm.push(complete(buffer)?);
        }
        println!(
            "{buffer:?}: after idle {:.1}, warm {:.1}",
            ms(median(&mut idle)),
            ms(median(&mut warm))
        );
    }
    // What "after idle" rebuilt: nothing may stay cached past the grace.
    client.end_input(SESSION)?;
    std::thread::sleep(GRACE * 4);
    let engine = futures::executor::block_on(client.diagnostics())?
        .engine
        .context("no engine")?;
    println!(
        "after idle: {} cached spec files, hook catalog loaded: {}",
        engine.registry.cached_file_count, engine.hook_catalog.loaded
    );
    Ok(())
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
