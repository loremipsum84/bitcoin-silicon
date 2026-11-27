"""Measure a running Bitcoin Core node in read-only mode."""

import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MIB = 1024**2
SCHEMA_VERSION = 2


def run_command(*args, timeout=30):
    result = subprocess.run(
        args, cwd=ROOT, env={**os.environ, "LC_ALL": "C"},
        text=True, capture_output=True, timeout=timeout,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()[-3000:]
        raise RuntimeError(f"{Path(args[0]).name} failed ({result.returncode}). {detail}")
    return result.stdout.strip()


def core_tool(name):
    directory = Path(os.environ.get("SILICON_BITCOIN_BIN_DIR", ROOT / ".local/bitcoin-core/bin"))
    executable = (ROOT / directory / name).resolve()
    if not os.access(executable, os.X_OK):
        raise RuntimeError(f"Could not find {name}. Run make setup or set SILICON_BITCOIN_BIN_DIR.")
    return executable


def rpc(chain, method):
    binary = ROOT / "target/release/bitcoin-silicon"
    if not os.access(binary, os.X_OK):
        raise RuntimeError("Could not find bitcoin-silicon. Run make build first.")
    return json.loads(run_command(binary, "node", "rpc", chain, method))


@dataclass(frozen=True)
class Sample:
    elapsed_seconds: float
    blocks: int
    headers: int
    verification_progress: float
    initial_block_download: bool
    best_block_hash: str
    received_bytes: int
    instance: str
    resources: dict = field(default_factory=dict)
    core_options: dict = field(default_factory=dict)


def interval(first, last):
    seconds = last.elapsed_seconds - first.elapsed_seconds
    blocks = last.blocks - first.blocks
    received = last.received_bytes - first.received_bytes
    issue = None
    if first.instance != last.instance:
        issue = "node_restarted"
    elif blocks < 0 or (blocks == 0 and first.best_block_hash != last.best_block_hash):
        issue = "chain_reorganized"
    elif received < 0:
        issue = "network_counter_reset"
    elif seconds <= 0:
        issue = "invalid_elapsed_time"
    return {
        "duration_seconds": seconds,
        "from_height": first.blocks,
        "to_height": last.blocks,
        "issue": issue,
        "blocks_per_second": blocks / seconds if issue is None else None,
        "received_mib_per_second": received / MIB / seconds if issue is None else None,
    }


def live_report(chain, samples):
    if len(samples) < 2:
        raise ValueError("Two samples are required.")
    intervals = [interval(first, last) for first, last in zip(samples, samples[1:])]
    summary = interval(samples[0], samples[-1])
    for measurement in intervals:
        if measurement["issue"]:
            summary.update(issue=measurement["issue"], blocks_per_second=None,
                           received_mib_per_second=None)
            break
    instances = {}
    rows = []
    for sample in samples:
        row = asdict(sample)
        row["instance"] = instances.setdefault(sample.instance, len(instances) + 1)
        rows.append(row)
    return {
        "schema_version": SCHEMA_VERSION,
        "mode": "live",
        "chain": chain,
        "summary": summary,
        "intervals": intervals,
        "samples": rows,
    }


def process_sample(pid_file):
    try:
        pid = int(pid_file.read_text())
        if pid <= 0:
            raise ValueError("PID must be positive")
        stats = run_command("/bin/ps", "-p", str(pid), "-o", "%cpu=", "-o", "rss=", "-o", "lstart=")
        cpu, rss, started = stats.split(maxsplit=2)
    except (OSError, ValueError, RuntimeError) as error:
        raise RuntimeError("Start the native Bitcoin Core node first.") from error
    return pid, started, {"cpu_percent": float(cpu), "rss_mib": int(rss) / 1024}


def node_sample(chain, clock_start):
    state = Path(os.environ.get("SILICON_STATE_DIR", ROOT / ".local"))
    data = ROOT / state / "bitcoin"
    if chain == "regtest":
        data /= chain
    pid_file = data / "bitcoind.pid"
    pid, started, resources = process_sample(pid_file)
    query_start = time.monotonic()
    info = rpc(chain, "getblockchaininfo")
    net = rpc(chain, "getnettotals")
    elapsed = (query_start + time.monotonic()) / 2 - clock_start
    command = run_command("/bin/ps", "-ww", "-p", str(pid), "-o", "command=")
    options = dict(re.findall(r"(?:^|\s)-(dbcache|par|prune)=(\d+)(?=\s|$)", command))
    if (pid, started) != process_sample(pid_file)[:2]:
        raise RuntimeError("The node restarted during sampling.")
    return Sample(
        elapsed, info["blocks"], info["headers"], info["verificationprogress"],
        info["initialblockdownload"], info["bestblockhash"], net["totalbytesrecv"],
        f"{pid}:{started}", resources, options,
    )


def measure(chain, duration, step):
    start = time.monotonic()
    samples = [node_sample(chain, start)]
    deadline = start + samples[0].elapsed_seconds + duration
    while True:
        next_sample = min(start + samples[-1].elapsed_seconds + step, deadline)
        time.sleep(max(0, next_sample - time.monotonic()))
        samples.append(node_sample(chain, start))
        if next_sample == deadline or start + samples[-1].elapsed_seconds >= deadline:
            report = live_report(chain, samples)
            report["core_version"] = rpc(chain, "getnetworkinfo")["subversion"]
            return report


def positive_seconds(value):
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("Duration must be positive")
    return seconds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("chain", choices=("mainnet", "regtest"), nargs="?", default="mainnet")
    parser.add_argument("--duration", type=positive_seconds, default=60, help="duration in seconds (60)")
    parser.add_argument("--interval", type=positive_seconds, default=10, help="interval in seconds (10)")
    parser.add_argument("--json", action="store_true", help="JSON report with all samples")
    args = parser.parse_args()
    report = measure(args.chain, args.duration, args.interval)
    summary = report["summary"]
    last = report["samples"][-1]
    if args.json:
        print(json.dumps(report, indent=2, allow_nan=False))
    elif summary["issue"]:
        print(f"Measurement interrupted ({summary['issue']}).", file=sys.stderr)
    else:
        print(f"{args.chain} · {summary['from_height']} → {summary['to_height']} · "
              f"{summary['duration_seconds']:.1f} s · {summary['blocks_per_second']:.2f} blocks/s · "
              f"{summary['received_mib_per_second']:.2f} MiB/s received")
        print(f"Verification {last['verification_progress']:.2%} · "
              f"CPU {last['resources']['cpu_percent']:.1f}% · "
              f"Resident memory {last['resources']['rss_mib']:.1f} MiB")
    return int(summary["issue"] is not None)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        sys.exit(str(error))
    except KeyboardInterrupt:
        sys.exit(130)
