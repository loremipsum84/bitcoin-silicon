mod benchmark;
mod core;
mod engine;
mod hash;
#[cfg(target_arch = "aarch64")]
mod hash_arm;
mod metal;
mod pool;
mod progress;
mod services;
mod solo;
mod wallet;
mod web;

use anyhow::{bail, ensure, Context, Result};
use core::{state_dir, Chain};
use engine::{configured_gpu_batch, default_threads, Engine, Mode};
use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
};

const HELP: &str = "Usage: bitcoin-silicon <command>

Services
  start         Start the mainnet node and dashboard in the background
  stop          Stop the dashboard and mainnet node
  status        Show dashboard and mainnet node status
  logs          Show recent dashboard output
  dashboard     Run the dashboard in the foreground
  node          Manage Bitcoin Core nodes and call RPC methods

Mining
  solo          Mine to a mainnet address
  pool          Mine via a Stratum pool
  demo          Mine one regtest block
  bench         Benchmark the mining engines

Diagnostics
  diagnose      Show system, Bitcoin Core and Metal information
  metal-test    Verify Metal SHA-256 test vectors

Run make setup for first-time installation.";

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_: libc::c_int) {
    SHUTDOWN.store(true, Relaxed);
}

fn shutdown_flag() -> Result<&'static AtomicBool> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // The handler only stores to a lock-free atomic.
        if unsafe { libc::signal(signal, request_shutdown as libc::sighandler_t) } == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(&SHUTDOWN)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(())
        }
        Some("start" | "stop" | "status" | "logs" | "diagnose") => {
            let action = args[0].as_str();
            ensure!(args.len() == 1, "Usage: bitcoin-silicon {action}");
            let state = state_dir();
            match action {
                "start" => services::start(&state, shutdown_flag()?),
                "stop" => services::stop(&state),
                "status" => services::status(&state),
                "logs" => services::logs(&state),
                "diagnose" => diagnose(&state),
                _ => unreachable!(),
            }
        }
        Some("wallet-create") => {
            ensure!(
                args.len() == 3,
                "Internal usage: wallet-create mainnet|regtest destination"
            );
            wallet::command(parse_chain(&args[1])?, std::path::Path::new(&args[2]))
        }
        Some("prepare-state") => core::prepare(&state_dir()),
        Some("node") => node_command(&args[1..]),
        Some("dashboard" | "dashboard-background") => {
            ensure!(args.len() == 1, "Usage: bitcoin-silicon dashboard");
            services::dashboard(
                state_dir(),
                shutdown_flag()?,
                args[0] == "dashboard-background",
            )
        }
        Some("bench") => benchmark::command(&args[1..]),
        Some("bench-solo") => benchmark::solo_command(&args[1..], shutdown_flag()?),
        Some("metal-test") => {
            ensure!(args.len() == 1, "Usage: bitcoin-silicon metal-test");
            metal_test()
        }
        Some("pool") => {
            ensure!(
                (3..=5).contains(&args.len()),
                "Usage: bitcoin-silicon pool host:port worker [gpu|hybrid|cpu] [threads]"
            );
            let mode = args
                .get(3)
                .map(|arg| Mode::parse(arg))
                .transpose()?
                .unwrap_or_default();
            let threads = args
                .get(4)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or_else(default_threads);
            let config = pool::PoolConfig::new(
                &args[1],
                args[2].clone(),
                std::env::var("SILICON_POOL_PASSWORD").ok(),
            )?;
            pool::run(
                config,
                Engine::start(mode, threads, configured_gpu_batch()?)?,
                shutdown_flag()?,
                &mut |status| {
                    println!(
                        "{}",
                        serde_json::to_string(status).expect("Progress must be serializable")
                    );
                },
            )
        }
        Some("solo") => {
            ensure!(
                (2..=4).contains(&args.len()),
                "Usage: bitcoin-silicon solo address [gpu|hybrid|cpu] [threads]"
            );
            let chain = Chain::Mainnet;
            let address = chain.address(&args[1])?;
            let mode = args
                .get(2)
                .map(|arg| Mode::parse(arg))
                .transpose()?
                .unwrap_or_default();
            let threads = args
                .get(3)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or_else(default_threads);
            solo::run(
                &state_dir(),
                chain,
                &address,
                Engine::start(mode, threads, configured_gpu_batch()?)?,
                shutdown_flag()?,
                &mut |status| {
                    println!(
                        "{}",
                        serde_json::to_string(status).expect("Progress must be serializable")
                    );
                },
            )
        }
        Some("demo") => {
            ensure!(
                args.len() <= 3,
                "Usage: bitcoin-silicon demo [gpu|hybrid|cpu] [threads]"
            );
            let mode = args
                .get(1)
                .map(|arg| Mode::parse(arg))
                .transpose()?
                .unwrap_or_default();
            let threads = args
                .get(2)
                .map(|arg| arg.parse())
                .transpose()?
                .unwrap_or_else(default_threads);
            let chain = Chain::Regtest;
            let address = chain.demo_address();
            let engine = Engine::start(mode, threads, configured_gpu_batch()?)?;
            solo::run(
                &state_dir(),
                chain,
                &address,
                engine,
                shutdown_flag()?,
                &mut |status| {
                    println!(
                        "{}",
                        serde_json::to_string(status).expect("Progress must be serializable")
                    );
                },
            )
        }
        Some(command) => bail!("Unknown command: {command}\n{HELP}"),
    }
}

fn metal_test() -> Result<()> {
    let gpu = metal::Gpu::new()?;
    metal::verify(&gpu)?;
    println!(
        "Metal: {} | Threadgroup size: {} | ARM SHA-256: {} | Test vectors passed",
        gpu.name,
        gpu.width,
        hash::arm_sha()
    );
    Ok(())
}

fn diagnose(state: &Path) -> Result<()> {
    for (program, args) in [
        ("sw_vers", &[][..]),
        (
            "sysctl",
            &["hw.model", "machdep.cpu.brand_string", "hw.memsize"][..],
        ),
        ("uname", &["-m"][..]),
        ("df", &["-h", "."][..]),
    ] {
        match Command::new(program).args(args).status() {
            Ok(status) if status.success() => (),
            Ok(status) => eprintln!("{program} exited with {status}"),
            Err(error) => eprintln!("Could not run {program}: {error}"),
        }
    }
    let bitcoin_version = (|| -> Result<()> {
        let output = Command::new(core::bitcoin_executable("bitcoind")?)
            .args(["-conf=/dev/null", "-nosettings", "-version"])
            .output()
            .context("Could not run bitcoind -version")?;
        ensure!(
            output.status.success(),
            "bitcoind exited with {}",
            output.status
        );
        println!(
            "{}",
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
        );
        Ok(())
    })();
    if let Err(error) = &bitcoin_version {
        eprintln!("{error:#}");
    }
    let status = services::status(state).and_then(|()| core::node_status(state, Chain::Regtest));
    let metal = metal_test();
    metal.and(status).and(bitcoin_version)
}

fn parse_chain(value: &str) -> Result<Chain> {
    match value {
        "mainnet" => Ok(Chain::Mainnet),
        "regtest" => Ok(Chain::Regtest),
        _ => bail!("Expected network: mainnet or regtest"),
    }
}

fn node_command(args: &[String]) -> Result<()> {
    let state = state_dir();
    match args {
        [action] if action == "status" => {
            for chain in [Chain::Mainnet, Chain::Regtest] {
                core::node_status(&state, chain)?;
            }
            Ok(())
        }
        [action] if action == "stop" => {
            let mainnet = core::node_stop(&state, Chain::Mainnet);
            let regtest = core::node_stop(&state, Chain::Regtest);
            mainnet.and(regtest)
        }
        [action] if action == "start" => core::node_start(&state, Chain::Mainnet),
        [action, chain] if action == "start" => core::node_start(&state, parse_chain(chain)?),
        [action, chain] if action == "stop" => core::node_stop(&state, parse_chain(chain)?),
        [action] if action == "logs" => core::node_logs(&state, Chain::Mainnet),
        [action, chain] if action == "logs" => core::node_logs(&state, parse_chain(chain)?),
        [action, chain, rpc @ ..] if action == "rpc" && !rpc.is_empty() =>
            core::node_cli(&state, parse_chain(chain)?, rpc),
        _ => bail!(
            "Usage: bitcoin-silicon node start|stop [mainnet|regtest], status, logs [mainnet|regtest], rpc mainnet|regtest method [arguments…]"
        ),
    }
}
