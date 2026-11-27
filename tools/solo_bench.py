#!/usr/bin/env python3
"""Compare engine and solo throughput with a loaded, offline regtest node."""

import argparse
from decimal import Decimal
import json
import os
from pathlib import Path
import socket
from struct import pack
import subprocess
from tempfile import TemporaryDirectory


FIXTURE_TRANSACTIONS = 20
OUTPUTS_PER_TRANSACTION = 1000
FULL_NONCE_RANGE = 1 << 32


def seed_mempool(rpc):
    address = rpc("decodescript", "51")["segwit"]["address"]
    script = bytes.fromhex(rpc("validateaddress", address)["scriptPubKey"])
    funding_blocks = rpc("generatetoaddress", 100 + FIXTURE_TRANSACTIONS, address)
    for block in funding_blocks[:FIXTURE_TRANSACTIONS]:
        coinbase = rpc("getblock", block, 2)["tx"][0]
        txid = bytes.fromhex(coinbase["txid"])[::-1]
        sats = int(coinbase["vout"][0]["value"] * 100_000_000)
        output_sats = (sats - 100_000) // OUTPUTS_PER_TRANSACTION
        output = pack("<Q", output_sats) + bytes([len(script)]) + script
        # A P2WSH OP_TRUE spend with one input and 1,000 outputs.
        header = pack("<I", 2) + b"\x00\x01\x01"
        tx_input = txid + pack("<I", 0) + b"\x00" + b"\xff" * 4
        outputs = b"\xfd" + pack("<H", OUTPUTS_PER_TRANSACTION) + output * OUTPUTS_PER_TRANSACTION
        witness = b"\x01\x01\x51"
        rpc("sendrawtransaction", (header + tx_input + outputs + witness + pack("<I", 0)).hex())


def compare(args):
    binary = args.binary.resolve(strict=True)
    options = [str(args.seconds), str(args.warmup), str(args.gpu_batch), args.profile]
    timeout = args.seconds + args.warmup + 90
    engine = json.loads(subprocess.check_output(
        [binary, "bench", *options, "--json"], timeout=timeout,
    ))

    with TemporaryDirectory(prefix="silicon-solo-bench-") as directory:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        env = dict(os.environ, SILICON_STATE_DIR=directory, SILICON_REGTEST_RPC_PORT=str(port))

        def node(action, *command, timeout=30):
            return subprocess.check_output(
                [binary, "node", action, "regtest", *map(str, command)],
                env=env, text=True, timeout=timeout,
            ).strip()

        def rpc(method, *parameters):
            result = node("rpc", method, *parameters)
            try:
                return json.loads(result, parse_float=Decimal)
            except json.JSONDecodeError:
                return result

        try:
            node("start")
            seed_mempool(rpc)
            tip = rpc("getbestblockhash")
            core_version = rpc("getnetworkinfo")["subversion"]
            transactions = rpc("getblocktemplate", '{"rules":["segwit"]}')["transactions"]
            if len(transactions) != FIXTURE_TRANSACTIONS:
                raise RuntimeError(f"Expected {FIXTURE_TRANSACTIONS} fixture transactions in the template.")
            transaction_weight = sum(tx["weight"] for tx in transactions)
            solo = json.loads(subprocess.check_output(
                [binary, "bench-solo", *options, str(args.nonce_count)], env=env, timeout=timeout,
            ))
            if rpc("getbestblockhash") != tip:
                raise RuntimeError("The regtest tip changed during the solo benchmark.")
            expected_template = {"transactions": len(transactions), "transaction_weight": transaction_weight}
            if not solo["templates"] or any(template != expected_template for template in solo["templates"]):
                raise RuntimeError("The solo benchmark did not use the loaded regtest template throughout.")
        finally:
            node("stop", timeout=125)

    return {
        "profile": args.profile,
        "warmup_seconds": args.warmup,
        "gpu_batch": args.gpu_batch,
        "core_version": core_version,
        "fixture_transactions": len(transactions),
        "fixture_transaction_weight": transaction_weight,
        "throughput_change_pct": (100 * (solo["useful_hs"] / engine["useful_hs"] - 1)
                                  if args.nonce_count == FULL_NONCE_RANGE else None),
        "engine": engine,
        "solo": solo,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/bitcoin-silicon"))
    parser.add_argument("--profile", default="gpu", help="engine profile, e.g. gpu, cpu:4, or hybrid:4")
    parser.add_argument("--seconds", type=int, default=60, help="measurement seconds per run (60)")
    parser.add_argument("--warmup", type=int, default=10, help="warm-up seconds per run (10)")
    parser.add_argument("--gpu-batch", type=int, default=1 << 20, help="nonces per GPU batch (1,048,576)")
    parser.add_argument("--nonce-count", type=int, default=FULL_NONCE_RANGE,
                        help="shorten the solo nonce range for smoke tests (default: 2^32)")
    args = parser.parse_args()
    if args.profile == "cpu-only":
        parser.error("--profile must select a single engine profile")
    if not 1 <= args.seconds <= 1800 or not 1 <= args.warmup <= 600:
        parser.error("--seconds must be between 1 and 1800 and --warmup between 1 and 600")
    if not 1 <= args.nonce_count <= FULL_NONCE_RANGE:
        parser.error("--nonce-count must be between 1 and 2^32")
    print(json.dumps(compare(args), indent=2))


if __name__ == "__main__":
    main()
