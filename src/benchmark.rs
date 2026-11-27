use crate::{
    engine::{configured_gpu_batch, default_threads, CpuQos, Engine, Mode},
    hash::{decode, Target, GENESIS},
    metal, solo,
};
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const USAGE: &str = "Usage: bitcoin-silicon bench [seconds] [warm-up] [gpu-batch] \
                    [cpu-only|cpu:N|gpu|hybrid:N] [--replace-ms milliseconds] [--json]";

#[derive(Clone, Copy, Serialize)]
struct Profile {
    mode: Mode,
    threads: usize,
}

impl Profile {
    fn from_arg(arg: &str) -> Result<Self> {
        let (mode, threads) = arg.split_once(':').unwrap_or((arg, ""));
        let mode = Mode::parse(mode)?;
        let threads = match mode {
            Mode::Gpu if threads.is_empty() => 0,
            Mode::Gpu => bail!("The GPU profile uses no CPU threads"),
            _ if threads.is_empty() => default_threads(),
            _ => threads.parse()?,
        };
        Ok(Self { mode, threads })
    }
}

#[derive(Serialize)]
struct Window {
    seconds: f64,
    cpu_hs: f64,
    gpu_hs: f64,
    total_hs: f64,
    thermal: Option<&'static str>,
}

#[derive(Serialize)]
struct Rate {
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
}

impl Rate {
    fn new(mean: f64, windows: &[Window], get: impl Fn(&Window) -> f64) -> Self {
        let mut rates: Vec<_> = windows.iter().map(get).collect();
        rates.sort_by(f64::total_cmp);
        let mid = rates.len() / 2;
        Self {
            mean,
            median: (rates[mid] + rates[(rates.len() - 1) / 2]) / 2.0,
            min: rates[0],
            max: rates[rates.len() - 1],
        }
    }
}

#[derive(Serialize)]
struct Summary {
    profile: Profile,
    cpu_qos: Option<CpuQos>,
    warmup_seconds: u64,
    seconds: f64,
    measurement_started_unix_ms: u128,
    measurement_ended_unix_ms: u128,
    replace_ms: Option<u64>,
    scheduled_replacements: u64,
    useful_hs: f64,
    cpu: Rate,
    gpu: Rate,
    total: Rate,
    cpu_nonces: u64,
    gpu_nonces: u64,
    nonces: u64,
    cancelled_gpu_nonces: u64,
    obsolete_candidates: u64,
    candidates: u64,
    valid: u64,
    cpu_verified: u64,
    cpu_replayed: u64,
    gpu_full_sha256d: u64,
    full_sha256d: u64,
    gpu_group: Option<usize>,
    gpu_batch: Option<u32>,
    gpu_last_batch: Option<u64>,
    gpu_batches: u64,
    gpu_timed_batches: u64,
    gpu_mean_ms: Option<f64>,
    gpu_max_ms: Option<f64>,
    thermal_before_warmup: Option<&'static str>,
    windows: Vec<Window>,
}

fn run_until(
    engine: &mut Engine,
    header: &mut [u8; 80],
    target: Target,
    end: Instant,
    schedule: &mut Option<(Instant, Duration)>,
) -> Result<u64> {
    let mut replacements = 0;
    loop {
        let now = Instant::now();
        if now >= end {
            return Ok(replacements);
        }
        let mut scheduled = false;
        if let Some((deadline, period)) = schedule {
            if now >= *deadline {
                scheduled = true;
                replacements += 1;
                *deadline += *period;
                if *deadline <= now {
                    // Skip missed ticks rather than publishing work workers could never start.
                    *deadline = now + *period;
                }
            }
        }
        if scheduled || engine.exhausted() {
            let time = u32::from_le_bytes(header[68..72].try_into()?);
            header[68..72].copy_from_slice(&time.wrapping_add(1).to_le_bytes());
            engine.set(*header, target, 0..1u64 << 32, false)?;
        }
        let wake_at = schedule.map_or(end, |(deadline, _)| deadline.min(end));
        engine.receive(
            wake_at
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5)),
        )?;
    }
}

fn measure(
    profile: Profile,
    seconds: u64,
    warm: u64,
    batch: u32,
    replace_ms: Option<u64>,
) -> Result<Summary> {
    let mut engine = Engine::start(profile.mode, profile.threads.max(1), batch)?;
    let mut header = decode(GENESIS)?;
    let target = Target::compact(0x1d00ffff)?;
    let thermal_before_warmup = metal::thermal_state();
    engine.set(header, target, 0..1u64 << 32, false)?;
    let warm_started = Instant::now();
    let period = replace_ms.map(Duration::from_millis);
    let mut schedule = period.map(|period| (warm_started + period, period));
    run_until(
        &mut engine,
        &mut header,
        target,
        warm_started + Duration::from_secs(warm),
        &mut schedule,
    )?;

    engine.reset_gpu_max();
    let start = engine.metrics();
    let started = Instant::now();
    let measurement_started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let mut schedule = period.map(|period| (started + period, period));
    let mut scheduled_replacements = 0;
    let mut at = started;
    let end = started + Duration::from_secs(seconds);
    let window_size = Duration::from_secs(if seconds >= 60 { 10 } else { 1 });
    let mut last = start;
    let mut windows = Vec::new();
    while at < end {
        // Merge a sub-window tail into the final sample.
        let limit = if end - at < window_size + window_size / 2 {
            end
        } else {
            at + window_size
        };
        scheduled_replacements +=
            run_until(&mut engine, &mut header, target, limit, &mut schedule)?;
        let counts = engine.metrics();
        let now = Instant::now();
        let elapsed = (now - at).as_secs_f64();
        let cpu_hs = (counts.cpu - last.cpu) as f64 / elapsed;
        let gpu_hs = (counts.gpu - last.gpu) as f64 / elapsed;
        windows.push(Window {
            seconds: elapsed,
            cpu_hs,
            gpu_hs,
            total_hs: cpu_hs + gpu_hs,
            thermal: metal::thermal_state(),
        });
        at = now;
        last = counts;
    }
    let measurement_ended = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    engine.check_error()?;
    let cpu_qos = engine.cpu_qos;
    let gpu_group = engine.gpu_width;
    let gpu_last_batch = engine.gpu_last_batch();
    // Shut down and drain any remaining commands outside the measured period.
    drop(engine);
    let seconds = (at - started).as_secs_f64();
    let end = last;
    let cpu_nonces = end.cpu - start.cpu;
    let gpu_nonces = end.gpu - start.gpu;
    let cpu_verified = end.cpu_verified - start.cpu_verified;
    let cpu_replayed = end.replayed - start.replayed;
    let gpu_full_sha256d = end.gpu_full - start.gpu_full;
    let gpu_timed_batches = end.gpu_timed_batches - start.gpu_timed_batches;
    let cpu = Rate::new(cpu_nonces as f64 / seconds, &windows, |w| w.cpu_hs);
    let gpu = Rate::new(gpu_nonces as f64 / seconds, &windows, |w| w.gpu_hs);
    let total = Rate::new(cpu.mean + gpu.mean, &windows, |w| w.total_hs);
    Ok(Summary {
        profile,
        cpu_qos,
        warmup_seconds: warm,
        seconds,
        measurement_started_unix_ms: measurement_started,
        measurement_ended_unix_ms: measurement_ended,
        replace_ms,
        scheduled_replacements,
        useful_hs: total.mean,
        cpu,
        gpu,
        total,
        cpu_nonces,
        gpu_nonces,
        nonces: cpu_nonces + gpu_nonces,
        cancelled_gpu_nonces: end.cancelled_gpu - start.cancelled_gpu,
        obsolete_candidates: end.obsolete - start.obsolete,
        candidates: end.candidates - start.candidates,
        valid: end.valid - start.valid,
        cpu_verified,
        cpu_replayed,
        gpu_full_sha256d,
        full_sha256d: cpu_nonces + cpu_verified + cpu_replayed + gpu_full_sha256d,
        gpu_group,
        gpu_batch: gpu_group.map(|_| batch),
        gpu_last_batch,
        gpu_batches: end.gpu_batches - start.gpu_batches,
        gpu_timed_batches,
        gpu_mean_ms: (gpu_timed_batches != 0)
            .then(|| (end.gpu_nanos - start.gpu_nanos) as f64 / gpu_timed_batches as f64 / 1e6),
        gpu_max_ms: (end.gpu_max_nanos != 0).then_some(end.gpu_max_nanos as f64 / 1e6),
        thermal_before_warmup,
        windows,
    })
}

pub fn command(args: &[String]) -> Result<()> {
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        println!("{USAGE}");
        println!("With no profile specified, run the CPU, GPU, and hybrid matrix.");
        println!("--replace-ms schedules work replacements in addition to normal rollover.");
        println!("--json writes one JSON object per profile.");
        return Ok(());
    }
    let mut json = false;
    let mut replace_ms = None;
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" if !json => json = true,
            "--replace-ms" if replace_ms.is_none() => {
                let value = args
                    .next()
                    .context("--replace-ms requires an interval in milliseconds")?
                    .parse()?;
                ensure!(
                    (1..=1_800_000).contains(&value),
                    "--replace-ms is out of range"
                );
                replace_ms = Some(value);
            }
            arg if arg.starts_with("--") => bail!("Unexpected benchmark option {arg}. {USAGE}"),
            _ => positional.push(arg.as_str()),
        }
    }
    let args = positional;
    ensure!(args.len() <= 4, "{USAGE}");
    let seconds = args
        .first()
        .map(|arg| arg.parse())
        .transpose()?
        .unwrap_or(30);
    let warm = args
        .get(1)
        .map(|arg| arg.parse())
        .transpose()?
        .unwrap_or(10);
    let batch = if let Some(value) = args.get(2) {
        value.parse()?
    } else {
        configured_gpu_batch()?
    };
    crate::engine::validate_gpu_batch(batch)?;
    ensure!(
        (1..=1800).contains(&seconds) && (1..=600).contains(&warm),
        "Benchmark durations are out of range"
    );

    let mut profiles: Vec<_> = [1, 2, 4, 8]
        .map(|threads| Profile {
            mode: Mode::Cpu,
            threads,
        })
        .into();
    match args.get(3).copied() {
        Some("cpu-only") => {}
        Some(profile) => profiles = vec![Profile::from_arg(profile)?],
        None => {
            profiles.push(Profile {
                mode: Mode::Gpu,
                threads: 0,
            });
            profiles.extend([1, 2, 4, 8].map(|threads| Profile {
                mode: Mode::Hybrid,
                threads,
            }));
        }
    }

    if !json {
        println!("engine,cpu_threads,seconds,cpu_H/s,gpu_H/s,total_H/s,median_H/s,min_H/s,max_H/s,nonces,candidates,valid,full_sha256d,gpu_batch,mean_batch_ms,max_batch_ms,thermal_state");
    }
    for profile in profiles {
        let summary = measure(profile, seconds, warm, batch, replace_ms)?;
        if json {
            println!("{}", serde_json::to_string(&summary)?);
        } else {
            println!(
                "{:?},{},{:.3},{:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{},{},{},{},{},{:.3},{:.3},{}",
                profile.mode,
                profile.threads,
                summary.seconds,
                summary.cpu.mean,
                summary.gpu.mean,
                summary.total.mean,
                summary.total.median,
                summary.total.min,
                summary.total.max,
                summary.nonces,
                summary.candidates,
                summary.valid,
                summary.full_sha256d,
                summary.gpu_batch.unwrap_or(0),
                summary.gpu_mean_ms.unwrap_or(0.0),
                summary.gpu_max_ms.unwrap_or(0.0),
                summary
                    .windows
                    .last()
                    .and_then(|w| w.thermal)
                    .unwrap_or("unavailable"),
            );
        }
    }
    Ok(())
}

pub fn solo_command(args: &[String], cancel: &AtomicBool) -> Result<()> {
    let [seconds, warmup, batch, profile, nonce_count] = args else {
        bail!("Internal usage: bench-solo seconds warm-up gpu-batch profile nonce-count");
    };
    let seconds = seconds.parse()?;
    let warmup = warmup.parse()?;
    ensure!(
        (1..=1800).contains(&seconds) && (1..=600).contains(&warmup),
        "Benchmark durations are out of range"
    );
    let nonce_count = nonce_count.parse()?;
    ensure!(
        (1..=1u64 << 32).contains(&nonce_count),
        "Benchmark nonce range is out of bounds"
    );
    let state = std::env::var_os("SILICON_STATE_DIR")
        .map(PathBuf::from)
        .context("bench-solo needs a SILICON_STATE_DIR for the isolated regtest node")?;
    let profile = Profile::from_arg(profile)?;
    let result = solo::benchmark(
        &state,
        Engine::start(profile.mode, profile.threads.max(1), batch.parse()?)?,
        cancel,
        Duration::from_secs(warmup),
        Duration::from_secs(seconds),
        nonce_count,
    )?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
