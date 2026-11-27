#!/usr/bin/env python3
"""Stratum V1 loopback fixture with independent share verification."""
import hashlib
import json
import queue
import socket
import struct
import subprocess
import sys
import threading
import time
from fractions import Fraction


PREVIOUS = bytes(range(32))
BRANCH = hashlib.sha256(b"fixture branch").digest()
COIN1 = bytes.fromhex("010000000100")
COIN2 = bytes.fromhex("ffff00000000")
VERSION, BITS = 0x20000000, 0x1D00FFFF
BASE_TARGET = 0xFFFF << (8 * (0x1D - 3))
WORKER = "fixture.worker"


def hash2(data):
    return hashlib.sha256(hashlib.sha256(data).digest()).digest()


def send(conn, message):
    conn.sendall(json.dumps(message).encode() + b"\n")


def reply(conn, request, result, error=None):
    send(conn, {"id": request["id"], "result": result, "error": error})


def notify(conn, method, params):
    send(conn, {"id": None, "method": method, "params": params})


def serve(listener, errors, done):
    try:
        seen = set()
        for phase in range(2):
            with listener.accept()[0] as conn, conn.makefile("rb") as stream:
                conn.settimeout(20)
                initial_prefix = bytes.fromhex("01020304")
                extra1 = initial_prefix
                jobs = {}
                counter = 0

                def job():
                    nonlocal counter, extra1
                    counter += 1
                    if phase == 0:
                        job_id = f"fixture-0-{counter}"
                        timestamp = 0x65010203 + counter
                        difficulty = Fraction(1, 5000 if counter == 1 else 2500)
                        previous = PREVIOUS if counter == 1 else PREVIOUS[::-1]
                        expected_extra = counter - 1
                    else:
                        job_id = ("fixture-0-2", "fixture-1-2", "fixture-1-3")[counter - 1]
                        timestamp = 0x65010205
                        difficulty = Fraction(1, 2500)
                        previous = PREVIOUS[::-1]
                        expected_extra = (2, 0, 3)[counter - 1]
                        extra1 = bytes.fromhex("05060708") if counter == 2 else initial_prefix
                    jobs[job_id] = timestamp, BASE_TARGET // difficulty, previous, extra1, expected_extra
                    # Phase 0 repeats the current prefix. Phase 1 switches away and back.
                    if counter > 1:
                        notify(conn, "mining.set_extranonce", [extra1.hex(), 4])
                    notify(conn, "mining.set_difficulty", [float(difficulty)])
                    notify(conn, "mining.notify", [job_id,
                        b"".join(previous[i:i+4][::-1] for i in range(0, 32, 4)).hex(),
                        COIN1.hex(), COIN2.hex(), [BRANCH.hex()], f"{VERSION:08x}",
                        f"{BITS:08x}", f"{timestamp:08x}", counter == 1])
                    return job_id

                authorized = False
                subscription = False
                sent = 0
                current = None
                while not done.is_set():
                    raw = stream.readline()
                    if not raw:
                        if phase == 1 and done.is_set():
                            return
                        raise AssertionError("miner disconnected early")
                    req = json.loads(raw)
                    method = req["method"]
                    if method == "mining.subscribe":
                        subscription = True
                        reply(conn, req, [[["mining.notify", "demo"]], extra1.hex(), 4])
                    elif method == "mining.authorize":
                        assert req["params"] == [WORKER, "x"]
                        authorized = True
                        reply(conn, req, True)
                    elif method == "mining.extranonce.subscribe":
                        reply(conn, req, True)
                    elif method == "mining.submit":
                        worker, job_id, extra, time_hex, nonce_hex = req["params"]
                        assert worker == WORKER and job_id in jobs
                        timestamp, target, previous, prefix, expected_extra = jobs[job_id]
                        assert time_hex == f"{timestamp:08x}" and len(nonce_hex) == 8
                        assert len(extra) == 8
                        coin = hash2(COIN1 + prefix + bytes.fromhex(extra) + COIN2)
                        root = hash2(coin + BRANCH)
                        header = (struct.pack("<I", VERSION) + previous + root
                                  + struct.pack("<III", timestamp, BITS, int(nonce_hex, 16)))
                        assert len(header) == 80
                        assert int.from_bytes(hash2(header), "little") <= target
                        assert header not in seen, "Duplicate header across jobs or connections"
                        seen.add(header)
                        if job_id != current:
                            reply(conn, req, False, [21, "stale", None])
                            continue
                        assert int(extra, 16) == expected_extra, "extranonce2 cursor reset"
                        sent += 1
                        if phase == 0 and counter == 1:
                            reply(conn, req, True)
                            current = job()
                        elif phase == 0 and sent == 2:
                            reply(conn, req, False, [23, "fixture rejection", None])
                        elif phase == 0 and sent == 3:
                            reply(conn, req, False, [21, "fixture stale", None])
                            notify(conn, "client.reconnect", [])
                            time.sleep(0.1)
                            break
                        else:
                            reply(conn, req, True)
                            if phase == 1 and counter < 3:
                                current = job()
                    else:
                        raise AssertionError(f"unexpected method {method}")
                    if current is None and authorized and subscription:
                        current = job()
    except (BrokenPipeError, ConnectionResetError):
        if not done.is_set():
            errors.put(AssertionError("miner disconnected early"))
    except Exception as error:
        errors.put(error)


def run(binary, mode):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(20)
        errors = queue.Queue()
        updates = queue.Queue()
        done = threading.Event()
        server = threading.Thread(target=serve, args=(listener, errors, done), daemon=True)
        server.start()
        with subprocess.Popen([binary, "pool", f"127.0.0.1:{listener.getsockname()[1]}", WORKER, mode, "4"],
                              stdout=subprocess.PIPE, text=True) as proc:
            def collect():
                for line in proc.stdout:
                    updates.put(json.loads(line))
                updates.put(None)
            reader = threading.Thread(target=collect, daemon=True)
            reader.start()
            try:
                until = time.monotonic() + 25
                saw_difficulty = False
                while time.monotonic() < until:
                    if not errors.empty():
                        raise errors.get()
                    try:
                        status = updates.get(timeout=0.2)
                    except queue.Empty:
                        continue
                    assert status is not None, "Stratum mining stopped"
                    saw_difficulty |= status.get("difficulty") in ("0.0004", "4e-4")
                    if (status.get("job") == "fixture-1-3" and status["accepted"] >= 4
                            and status["rejected"] >= 1 and status["stale"] >= 1):
                        assert saw_difficulty and status["block_changes"] >= 1
                        print(f"{mode}: Verified shares, tip change, difficulty, rejections, "
                              "stale shares, same-job reconnect, and prefix rotation")
                        return
                raise AssertionError(f"Incomplete Stratum validation ({mode})")
            finally:
                done.set()
                proc.terminate()
                proc.wait(timeout=5)


if __name__ == "__main__":
    for selected in sys.argv[2:] or ["cpu"]:
        run(sys.argv[1], selected)
