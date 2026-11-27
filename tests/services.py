#!/usr/bin/env python3
"""Exercise the combined CLI lifecycle with temporary, offline Bitcoin Core nodes."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
from shlex import quote
import socket
import stat
import subprocess
import sys
from tempfile import TemporaryDirectory
import time

from web_api import HOST, ORIGIN, PORT, request, run as check_web_api


ROOT = Path(__file__).resolve().parents[1]


def listener(port=0):
    server = socket.socket()
    try:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind((HOST, port))
        server.listen()
    except OSError:
        server.close()
        raise
    return server


def eventually(check):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.1)
    raise AssertionError("Timed out waiting for the service state")


def run(binary):
    core = Path(os.environ.get("SILICON_BITCOIN_BIN_DIR", ROOT / ".local/bitcoin-core/bin")).resolve()
    with TemporaryDirectory(prefix="silicon-services-") as temporary:
        folder = Path(temporary)
        offline = folder / "offline-core"
        offline.mkdir()
        daemon = offline / "bitcoind"
        daemon.write_text(f'#!/bin/sh\nexec {quote(str(core / "bitcoind"))} "$@" -networkactive=0\n')
        daemon.chmod(0o700)
        (offline / "bitcoin-cli").symlink_to(core / "bitcoin-cli")

        with listener() as mainnet, listener() as regtest:
            base = dict(os.environ, SILICON_BITCOIN_BIN_DIR=str(offline), SILICON_MAINNET_DBCACHE="32",
                        SILICON_MAINNET_RPC_PORT=str(mainnet.getsockname()[1]),
                        SILICON_REGTEST_RPC_PORT=str(regtest.getsockname()[1]))

        def isolated(name):
            return dict(base, SILICON_STATE_DIR=str(folder / name))

        def cli(env, *args, check=True):
            result = subprocess.run([binary, *args], cwd=ROOT, env=env, text=True,
                                    capture_output=True, timeout=150)
            if check:
                assert result.returncode == 0, (args, result.stdout, result.stderr)
            return result

        def assert_stopped(env):
            status = cli(env, "status").stdout
            assert "dashboard: stopped" in status and "mainnet: stopped" in status, status

        collision = isolated("port-collision")
        with listener(PORT):
            result = cli(collision, "start", check=False)
            assert result.returncode and "Dashboard port" in result.stderr, result.stderr
            assert_stopped(collision)
            assert not (folder / "port-collision/bitcoin/debug.log").exists()

        failure = isolated("node-failure") | {"SILICON_MAINNET_DBCACHE": "invalid"}
        try:
            result = cli(failure, "start", check=False)
            assert result.returncode and "SILICON_MAINNET_DBCACHE" in result.stderr, result.stderr
            assert_stopped(failure)
            assert not (folder / "node-failure/dashboard.token").exists()

            # A foreground dashboard already owned by this state survives the same node failure.
            with subprocess.Popen([binary, "dashboard"], cwd=ROOT, env=failure,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as foreground:
                try:
                    eventually(lambda: "dashboard: running" in cli(failure, "status").stdout)
                    result = cli(failure, "start", check=False)
                    assert result.returncode and "SILICON_MAINNET_DBCACHE" in result.stderr, result.stderr
                    assert foreground.poll() is None
                    assert "dashboard: running" in cli(failure, "status").stdout
                finally:
                    try:
                        cli(failure, "stop")
                        assert foreground.wait(timeout=10) == 0
                    finally:
                        if foreground.poll() is None:
                            foreground.kill()
        finally:
            cli(failure, "stop")

        state = isolated("active")
        try:
            with ThreadPoolExecutor(max_workers=2) as pool:
                starts = [pool.submit(cli, state, "start") for _ in range(2)]
                for start in starts:
                    assert ORIGIN in start.result().stdout
            status = cli(state, "status").stdout
            assert "dashboard: running" in status and "mainnet: running, 0 blocks / 0 headers" in status, status
            assert cli(state, "logs").stdout.count(ORIGIN) == 1
            info = json.loads(cli(state, "node", "rpc", "mainnet", "getnetworkinfo").stdout)
            assert info["networkactive"] is False

            # Reusing the same port with a different state must never control this instance.
            other = isolated("other-state")
            result = cli(other, "start", check=False)
            assert result.returncode and "Dashboard port" in result.stderr, result.stderr
            cli(other, "stop")
            assert "dashboard: running" in cli(state, "status").stdout

            control = folder / "active/dashboard.token"
            assert stat.S_IMODE(control.stat().st_mode) == 0o600
            assert request("POST", "/_silicon/stop", headers={
                "Origin": ORIGIN, "X-Silicon-Control": control.read_text(),
            })[0] == 403
            check_web_api(demo=True)

            _, dashboard = request("GET", "/api/state")
            headers = {"Content-Type": "application/json", "Origin": ORIGIN,
                       "X-Silicon-Token": dashboard["csrf"]}
            decoded = json.loads(cli(state, "node", "rpc", "mainnet", "decodescript", "51").stdout)
            address = decoded["segwit"]["address"]
            assert request("POST", "/api/address", {"chain": "mainnet", "address": address}, headers)[0] == 200
            result = request("POST", "/api/start", {"route": "mainnet", "mode": "cpu", "threads": 1,
                                                    "address": address}, headers)
            assert result[0] == 200, result

            def mining_waits_for_sync():
                runtime = request("GET", "/api/state")[1]["runtime"]
                return runtime["active"] and runtime["progress"]["message"] == "Bitcoin Core is performing its initial sync"

            eventually(mining_waits_for_sync)
            cli(state, "stop")
            cli(state, "stop")
            assert_stopped(state)
            assert not control.exists()
            assert not (folder / "active/bitcoin/bitcoind.pid").exists()
            assert "regtest: running" in cli(state, "node", "status").stdout
        finally:
            cli(state, "stop")
            cli(state, "node", "stop", "regtest")

    print("Verified concurrent start, port isolation, rollback, and clean stop during a mining session.")


if __name__ == "__main__":
    executable = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target/release/bitcoin-silicon"
    run(str(executable.resolve()))
