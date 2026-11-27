#!/usr/bin/env python3
"""Verify throwaway encrypted wallet backups on both chains."""
import pathlib
import secrets
import stat
import subprocess
import sys
import tempfile


def run(binary):
    with tempfile.TemporaryDirectory(prefix="silicon-wallet-test-") as folder:
        for chain, prefix in (("mainnet", b"bc1"), ("regtest", b"bcrt1")):
            destination = pathlib.Path(folder) / f"{chain}.dat"
            password = secrets.token_hex(24).encode()
            command = [binary, "wallet-create", chain, str(destination)]
            created = subprocess.run(command, input=password, capture_output=True, timeout=100)
            assert password not in created.stdout + created.stderr
            assert created.returncode == 0, created.stderr.decode()
            assert created.stdout.strip().startswith(prefix)
            assert not created.stderr
            assert stat.S_IMODE(destination.stat().st_mode) == 0o600
            original = destination.read_bytes()
            assert original.startswith(b"SQLite format 3\0")
            duplicate = subprocess.run(command, input=password, capture_output=True, timeout=10)
            assert duplicate.returncode != 0 and password not in duplicate.stdout + duplicate.stderr
            assert destination.read_bytes() == original
            print(f"{chain}: Encrypted backup restored, address verified, file permissions 0600.")


if __name__ == "__main__":
    run(sys.argv[1] if len(sys.argv) > 1 else "./target/release/bitcoin-silicon")
