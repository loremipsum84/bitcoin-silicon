#!/usr/bin/env python3
"""Add a witness transaction to regtest and assert that the miner includes it."""
import json
import struct
import subprocess
import sys
from decimal import Decimal


def run(binary):
    rpc_command = [binary, "node", "rpc", "regtest"]

    def rpc(method, *args):
        value = subprocess.check_output([*rpc_command, method, *map(str, args)], text=True).strip()
        try:
            return json.loads(value, parse_float=Decimal)
        except json.JSONDecodeError:
            return value

    subprocess.run([binary, "node", "start", "regtest"], check=True)
    rpc("-rpcwait", "getblockcount")
    address = rpc("decodescript", "51")["segwit"]["address"]
    script = bytes.fromhex(rpc("validateaddress", address)["scriptPubKey"])
    # Core generates only the fixture funding blocks. The tested block is
    # constructed, hashed and submitted by Silicon Miner itself.
    funding_blocks = rpc("generatetoaddress", 101, address)
    funding = rpc("getblock", funding_blocks[0], 2)["tx"][0]
    txid = bytes.fromhex(funding["txid"])[::-1]
    value = round(funding["vout"][0]["value"] * 100_000_000) - 1000
    tx = (struct.pack("<I", 2) + b"\x00\x01\x01" + txid + struct.pack("<I", 0)
          + b"\x00" + struct.pack("<I", 0xffffffff) + b"\x01"
          + struct.pack("<Q", value) + bytes([len(script)]) + script
          + b"\x01\x01\x51" + struct.pack("<I", 0))
    expected = rpc("sendrawtransaction", tx.hex())
    output = subprocess.check_output([binary, "demo", "cpu", "2"], text=True, timeout=75)
    status = json.loads(output.splitlines()[-1])
    assert status["accepted"] == 1 and status["node"]["blocks"] == status["height"], status
    block = rpc("getblock", status["hash"], 2)
    assert status["hash"] == rpc("getblockhash", status["height"])
    assert block["tx"][0]["vout"][0]["scriptPubKey"]["address"] == address
    reward = status["reward"]
    assert reward["block_hash"] == status["hash"] and reward["height"] == status["height"]
    assert reward["txid"] == block["tx"][0]["txid"]
    payout = rpc("gettxout", reward["txid"], 0)
    assert payout["coinbase"] and payout["confirmations"] == 1
    assert payout["scriptPubKey"]["address"] == address
    assert payout["value"] * 100_000_000 == reward["sats"]
    assert block["tx"][0]["vin"][0]["txinwitness"] == ["00" * 32]
    assert block["tx"][0]["vout"][1]["scriptPubKey"]["hex"].startswith("6a24aa21a9ed")
    matches = [entry for entry in block["tx"] if entry["txid"] == expected]
    assert len(matches) == 1 and matches[0]["vin"][0]["txinwitness"] == ["51"]
    print(f"SegWit block accepted at height {status['height']}: {status['hash']}")
    print(f"Witness transaction included: {expected}")
    print(f"Coinbase reward verified for {address}: {reward['sats']} satoshis")


if __name__ == "__main__":
    run(sys.argv[1] if len(sys.argv) > 1 else "./target/release/bitcoin-silicon")
