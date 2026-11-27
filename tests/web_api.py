#!/usr/bin/env python3
"""Run against bitcoin-silicon dashboard to check loopback security and the regtest action."""
import http.client
import json
import sys
import time

HOST = "127.0.0.1"
PORT = 7878
ORIGIN = f"http://{HOST}:{PORT}"


def request(method, uri, value=None, headers=None):
    connection = http.client.HTTPConnection(HOST, PORT, timeout=5)
    try:
        connection.request(method, uri, None if value is None else json.dumps(value), headers or {})
        response = connection.getresponse()
        data = response.read()
        assert response.getheader("Access-Control-Allow-Origin") is None
        assert response.getheader("X-Frame-Options") == "DENY"
        return response.status, json.loads(data)
    finally:
        connection.close()


def run(demo):
    status, state = request("GET", "/api/state")
    assert status == 200 and state["runtime"]["active"] is None
    assert "password" not in json.dumps(state).lower()
    assert 1 <= state["system"]["default_gpu_batch"] <= state["system"]["max_gpu_batch"]
    assert 1 <= state["system"]["default_threads"] <= state["system"]["max_threads"]
    assert state["system"]["default_mode"] == ("gpu" if state["system"]["metal"] else "cpu")
    if state["system"]["metal"]:
        assert state["system"]["gpu_group"] > 0
    status, _ = request("GET", "/api/state", headers={"Host": "example.test:7878"})
    assert status == 403
    for route in ("status", "stop"):
        status, _ = request("POST", f"/_silicon/{route}", headers={"X-Silicon-Control": state["csrf"]})
        assert status == 403
    headers = {"Content-Type": "application/json", "Origin": ORIGIN, "X-Silicon-Token": state["csrf"]}
    start = {"route": "regtest", "mode": "cpu", "threads": 2}
    for replace in ({"Origin": "http://example.test"}, {"X-Silicon-Token": "bad"},
                    {"Sec-Fetch-Site": "cross-site"}, {"Content-Type": "text/plain"}):
        status, _ = request("POST", "/api/start", start, headers | replace)
        assert status == 403, (status, replace)
    status, _ = request("POST", "/api/start", start | {"threads": 0}, headers)
    assert status == 400
    for batch in [0, state["system"]["max_gpu_batch"] + 1]:
        status, _ = request("POST", "/api/start", start | {"gpu_batch": batch}, headers)
        assert status == 400
    status, _ = request("POST", "/api/address", {"chain": "mainnet", "address": state["demo_address"]}, headers)
    assert status == 400
    status, _ = request("POST", "/api/balance", {"chain": "regtest"}, headers | {"X-Silicon-Token": "bad"})
    assert status == 403
    if demo:
        status, result = request("POST", "/api/start", start, headers)
        assert status == 200 and result["ok"], result
        until = time.monotonic() + 70
        while time.monotonic() < until:
            _, update = request("GET", "/api/state")
            active, progress = update["runtime"]["active"], update["runtime"]["progress"]
            if active is None:
                assert progress["accepted"] == 1 and progress["hash"], progress
                assert progress["block_changes"] == 1
                assert progress["previous_block"] == progress["hash"]
                assert progress["reward"]["block_hash"] == progress["hash"]
                print(f"API demo: Block {progress['height']}, {progress['hash']}")
                break
            time.sleep(.2)
        else:
            raise AssertionError("Dashboard demo timed out")
        status, result = request("POST", "/api/balance", {"chain": "regtest"}, headers)
        assert status == 200 and result["ok"], result
        until = time.monotonic() + 10
        while time.monotonic() < until:
            _, update = request("GET", "/api/state")
            balance = update["runtime"]["balance"]
            if not balance["busy"]:
                assert balance["result"]["immature_sats"] >= progress["reward"]["sats"], balance
                assert balance["reward"]["confirmations"] == 1, balance
                assert balance["reward"]["block"]["txid"] == progress["reward"]["txid"], balance
                break
            time.sleep(.2)
        else:
            raise AssertionError("Regtest balance scan timed out")
    print("Rejected invalid hosts, origins, tokens, content types, and parameters.")


if __name__ == "__main__":
    run("--demo" in sys.argv)
