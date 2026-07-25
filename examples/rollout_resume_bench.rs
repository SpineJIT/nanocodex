use std::{env, path::PathBuf, time::Instant};

use eyre::{Result, WrapErr, eyre};
use nanocodex::{Nanocodex, RolloutConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let codex_home = PathBuf::from(args.next().ok_or_else(|| eyre!("missing CODEX_HOME"))?);
    let iterations = args
        .next()
        .ok_or_else(|| eyre!("missing iteration count"))?
        .parse::<usize>()
        .wrap_err("iteration count must be an integer")?;
    let thread_ids = args.collect::<Vec<_>>();
    if thread_ids.is_empty() {
        return Err(eyre!("provide at least one thread ID"));
    }

    let config = RolloutConfig::new(codex_home);
    for thread_id in thread_ids {
        let mut samples = Vec::with_capacity(iterations);
        let mut history_items = 0;
        let mut visible_messages = 0;
        let mut rollout_bytes = 0;
        let mut loaded = None;
        for _ in 0..iterations {
            let started = Instant::now();
            let session = config
                .load_session(&thread_id)
                .wrap_err_with(|| format!("failed to load {thread_id}"))?;
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
            rollout_bytes = session.rollout_path().metadata()?.len();
            history_items = serde_json::to_value(session.snapshot())?["history"]
                .as_array()
                .map_or(0, Vec::len);
            visible_messages = session.messages().len();
            loaded = Some(session);
        }
        let session = loaded.ok_or_else(|| eyre!("iteration count must be positive"))?;
        let (agent, events) = Nanocodex::builder("benchmark-only")
            .resume(session.snapshot().clone())
            .build()
            .wrap_err_with(|| format!("failed to construct resumed driver for {thread_id}"))?;
        drop((agent, events));
        samples.sort_by(f64::total_cmp);
        let median = samples[samples.len() / 2];
        let sample_count = u32::try_from(samples.len()).wrap_err("too many benchmark samples")?;
        let mean = samples.iter().sum::<f64>() / f64::from(sample_count);
        println!(
            "{thread_id}\tbytes={rollout_bytes}\thistory={history_items}\tmessages={visible_messages}\tvalidated=true\tmin_ms={:.3}\tp50_ms={median:.3}\tmean_ms={mean:.3}\tmax_ms={:.3}",
            samples[0],
            samples[samples.len() - 1]
        );
    }
    Ok(())
}
