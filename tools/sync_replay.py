"""Replay a deterministic regtest chain on temporary, isolated nodes."""

import argparse
from datetime import datetime
from hashlib import sha256
import json
import math
import os
import platform
from pathlib import Path
import re
from statistics import median
from struct import pack
import subprocess
import sys
from tempfile import TemporaryDirectory

from sync_bench import SCHEMA_VERSION, core_tool, run_command


CHECKPOINT = 100
DEFAULT_CACHE_MIB = 450
PRUNE_MIB = 2048
GENESIS = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206"
GENESIS_TIME = 1296688602
REGTEST_MAGIC = bytes.fromhex("fabfb5da")
REGTEST_BITS = 0x207FFFFF
REGTEST_TARGET = 0x7FFFFF << (8 * (0x20 - 3))


def hash256(payload):
    return sha256(sha256(payload).digest()).digest()


def compact_size(value):
    return bytes([value]) if value < 253 else b"\xfd" + pack("<H", value)


def height_script(height):
    if height <= 16:
        return bytes([0x50 + height, 0])  # Minimum coinbase script length is two bytes.
    encoded = height.to_bytes((height.bit_length() + 8) // 8, "little")
    return bytes([len(encoded)]) + encoded


def transaction(prev, index, script, outputs, count):
    return (pack("<I", 1) + b"\x01" + prev + pack("<I", index) + compact_size(len(script))
            + script + b"\xff" * 4 + compact_size(count) + outputs + b"\x00" * 4)


def output(satoshis):
    return pack("<Q", satoshis) + b"\x01\x51"  # Length-prefixed OP_TRUE script.


def write_blocks(stream, previous, first, last, funding=None, outputs_per_block=0):
    for height in range(first, last + 1):
        subsidy = 5_000_000_000 >> (height // 150)
        coinbase = transaction(bytes(32), 0xFFFFFFFF, height_script(height), output(subsidy), 1)
        coinbase_hash = hash256(coinbase)
        if height == 1:
            funding = coinbase_hash
        merkle = coinbase_hash
        txs = b"\x01" + coinbase
        if outputs_per_block:
            # Roll the change forward, leaving the other outputs in the UTXO set.
            change = 5_000_000_000 - (height - CHECKPOINT) * outputs_per_block
            outputs = output(change) + output(1) * outputs_per_block
            spend = transaction(funding, 0, b"", outputs, outputs_per_block + 1)
            funding = hash256(spend)
            merkle = hash256(coinbase_hash + funding)
            txs = b"\x02" + coinbase + spend
        prefix = pack("<I", 4) + previous + merkle + pack("<II", GENESIS_TIME + 600 * height, REGTEST_BITS)
        for nonce in range(2**32):
            header = prefix + pack("<I", nonce)
            previous = hash256(header)
            if int.from_bytes(previous, "little") <= REGTEST_TARGET:
                break
        else:
            raise RuntimeError(f"Could not find proof of work for block {height}.")
        stream.write(REGTEST_MAGIC + pack("<I", len(header) + len(txs)) + header + txs)
    return previous, funding


def fixture(directory, blocks, outputs_per_block):
    with (directory / "checkpoint.dat").open("wb") as stream:
        checkpoint, funding = write_blocks(stream, bytes.fromhex(GENESIS)[::-1], 1, CHECKPOINT)
    with (directory / "blocks.dat").open("wb") as stream:
        tip, _ = write_blocks(stream, checkpoint, CHECKPOINT + 1, CHECKPOINT + blocks,
                              funding, outputs_per_block)
    return {
        "checkpoint_height": CHECKPOINT,
        "checkpoint_hash": checkpoint[::-1].hex(),
        "target_height": CHECKPOINT + blocks,
        "target_hash": tip[::-1].hex(),
        "blocks": blocks,
        "outputs_per_block": outputs_per_block,
        "fixture_bytes": (directory / "blocks.dat").stat().st_size,
    }


def read_import(log, expected_start, expected_end):
    first_tip = re.search(r"Loaded best chain: hashBestChain=([0-9a-f]{64}) height=(\d+)", log)
    tips = re.findall(r"UpdateTip: new best=([0-9a-f]{64}) height=(\d+)", log)
    if expected_start is not None and (not first_tip or first_tip.groups() != expected_start):
        raise RuntimeError("The test node did not resume at the expected checkpoint.")
    if not tips or tips[-1] != expected_end:
        raise RuntimeError(f"The test node did not reach the expected block.\n{log[-3000:]}")
    timestamps = [re.search(rf"^(\S+) {marker}", log, re.MULTILINE)
                  for marker in ("Importing blocks file", "Stopping after block import", "Shutdown done")]
    if not all(timestamps):
        raise RuntimeError(f"The replay did not produce the expected measurement boundaries.\n{log[-3000:]}")
    start, imported, shutdown = [datetime.strptime(match[1], "%Y-%m-%dT%H:%M:%S.%fZ")
                                 for match in timestamps]
    seconds = (shutdown - start).total_seconds()
    if not start < imported <= shutdown:
        raise RuntimeError("Inconsistent timestamp during replay.")
    cache = re.findall(r"UpdateTip: .*? cache=([\d.]+)MiB", log)
    return {
        "duration_seconds": seconds,
        "import_seconds": (imported - start).total_seconds(),
        "peak_utxo_cache_mib": max(map(float, cache), default=None),
        "resumed_from_hash": first_tip[1] if first_tip else None,
        "resumed_from_height": int(first_tip[2]) if first_tip else None,
        "final_hash": tips[-1][0],
        "final_height": int(tips[-1][1]),
    }


def import_blocks(bitcoind, datadir, blocks, cache, expected_start, expected_end):
    log = run_command(
        bitcoind, "-conf=/dev/null", "-nosettings", f"-datadir={datadir}", "-regtest",
        "-server=0", "-networkactive=0", "-listen=0", "-listenonion=0",
        "-dnsseed=0", "-discover=0", "-disablewallet=1",
        # Match mainnet: the exhaustive regtest block-index assertion runs per block.
        "-checkblockindex=0", "-logtimemicros=1", "-printtoconsole=1", "-stopafterblockimport=1",
        f"-loadblock={blocks}", f"-dbcache={cache}", f"-prune={PRUNE_MIB}",
        timeout=600,
    )
    return read_import(log, expected_start, expected_end)


def replay_once(bitcoind, directory, manifest, cache):
    with TemporaryDirectory(prefix="silicon-sync-node-", dir=directory) as temporary:
        datadir = Path(temporary)
        checkpoint = manifest["checkpoint_hash"], str(manifest["checkpoint_height"])
        target = manifest["target_hash"], str(manifest["target_height"])
        import_blocks(bitcoind, datadir, directory / "checkpoint.dat", DEFAULT_CACHE_MIB, None, checkpoint)
        trial = import_blocks(bitcoind, datadir, directory / "blocks.dat", cache, checkpoint, target)
        trial["blocks_per_second"] = manifest["blocks"] / trial["duration_seconds"]
        return trial


def compare(baseline, candidate, max_regression_pct):
    def summarize(trials):
        rates = [trial["blocks_per_second"] for trial in trials]
        midpoint = median(rates)
        return {"median_blocks_per_second": midpoint, "median_absolute_deviation": median(
            abs(rate - midpoint) for rate in rates), "trials": trials}

    old, new = summarize(baseline), summarize(candidate)
    return {"baseline": old, "candidate": new,
            **compare_rates(old, new, max_regression_pct)}


def compare_rates(baseline, candidate, max_regression_pct):
    old = baseline["median_blocks_per_second"]
    new = candidate["median_blocks_per_second"]
    if not (math.isfinite(old) and math.isfinite(new) and old > 0 and new > 0):
        raise ValueError("Invalid replay throughput.")
    change = 100 * (new / old - 1)
    return {
        "throughput_change_pct": change,
        "max_regression_pct": max_regression_pct,
        "regression": max_regression_pct is not None and 100 * (old - new) > old * max_regression_pct,
    }


def reference_candidate(reference, manifest, environment, cache):
    if (not isinstance(reference, dict) or reference.get("schema_version") != SCHEMA_VERSION
            or reference.get("mode") != "replay"):
        raise ValueError("Incompatible reference report.")
    candidate = reference.get("candidate", {})
    if reference.get("fixture") != manifest or reference.get("environment") != environment:
        raise ValueError("The reference must use the same test chain, host, and Bitcoin Core version.")
    if not isinstance(candidate, dict) or candidate.get("cache_mib") != cache:
        raise ValueError("The reference must use the same candidate cache size.")
    rate = candidate.get("median_blocks_per_second")
    if type(rate) not in (int, float) or not math.isfinite(rate) or rate <= 0:
        raise ValueError("Invalid reference throughput.")
    return candidate


def bounded_int(lower, upper):
    def parse(value):
        number = int(value)
        if not lower <= number <= upper:
            raise argparse.ArgumentTypeError(f"expected a number between {lower} and {upper}")
        return number
    return parse


def percentage(value):
    number = float(value)
    if not math.isfinite(number) or not 0 <= number <= 100:
        raise argparse.ArgumentTypeError("expected a percentage between 0 and 100")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blocks", type=bounded_int(1, 20_000), default=6_000, help="blocks to replay (6,000)")
    parser.add_argument("--outputs-per-block", type=bounded_int(0, 4_096), default=2_048,
                        help="unspent outputs added per block (2,048)")
    parser.add_argument("--repeats", type=bounded_int(1, 20), default=3, help="trials per setting (3)")
    parser.add_argument("--baseline-cache", type=bounded_int(4, 16_384), default=DEFAULT_CACHE_MIB,
                        help="baseline cache in MiB (450)")
    parser.add_argument("--candidate-cache", type=bounded_int(4, 16_384), default=4_096, help="candidate cache in MiB (4,096)")
    parser.add_argument("--reference", type=Path, help="prior JSON report for the same candidate setting")
    parser.add_argument("--max-regression-pct", type=percentage, help="maximum allowed throughput decrease, in %%")
    parser.add_argument("--json", action="store_true", help="JSON report with all trials")
    args = parser.parse_args()

    bitcoind = core_tool("bitcoind")
    environment = {
        "architecture": platform.machine(),
        "macos": platform.mac_ver()[0],
        "cpu_count": os.cpu_count(),
        "memory_bytes": int(run_command("/usr/sbin/sysctl", "-n", "hw.memsize")),
        "core_version": run_command(bitcoind, "-conf=/dev/null", "-nosettings", "-version").splitlines()[0],
        "prune_mib": PRUNE_MIB,
    }
    profiles = [(args.baseline_cache, []), (args.candidate_cache, [])]
    reference = None
    with TemporaryDirectory(prefix="silicon-sync-fixture-") as temporary:
        directory = Path(temporary).resolve()
        manifest = fixture(directory, args.blocks, args.outputs_per_block)
        if args.reference:
            reference = reference_candidate(
                json.loads(args.reference.read_text()), manifest, environment, args.candidate_cache,
            )
        for repetition in range(args.repeats):
            order = (0, 1) if repetition % 2 == 0 else (1, 0)
            for profile in order:
                cache, trials = profiles[profile]
                print(f"Replay {repetition + 1}/{args.repeats} · {cache} MiB cache", file=sys.stderr)
                trials.append(replay_once(bitcoind, directory, manifest, cache))

    comparison = compare(profiles[0][1], profiles[1][1], None if reference else args.max_regression_pct)
    comparison["baseline"]["cache_mib"] = args.baseline_cache
    comparison["candidate"]["cache_mib"] = args.candidate_cache
    report = {"schema_version": SCHEMA_VERSION, "mode": "replay",
              "environment": environment, "fixture": manifest, **comparison}
    outcome = comparison
    if reference:
        outcome = compare_rates(reference, comparison["candidate"], args.max_regression_pct)
        report["against_reference"] = {"reference_blocks_per_second": reference["median_blocks_per_second"],
                                       **outcome}
    if args.json:
        print(json.dumps(report, indent=2, allow_nan=False))
    else:
        for profile in ("baseline", "candidate"):
            result = report[profile]
            print(f"{result['cache_mib']} MiB · Median {result['median_blocks_per_second']:.2f} blocks/s · "
                  f"Median absolute deviation {result['median_absolute_deviation']:.2f} blocks/s")
        print(f"Throughput change {report['throughput_change_pct']:+.2f}%")
        if reference:
            print(f"Throughput change from the reference report {outcome['throughput_change_pct']:+.2f}%")
        if outcome["regression"]:
            print("Regression exceeds the threshold.", file=sys.stderr)
    return int(outcome["regression"])


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        sys.exit(str(error))
    except KeyboardInterrupt:
        sys.exit(130)
