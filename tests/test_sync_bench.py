import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

from sync_bench import MIB, Sample, live_report
from sync_replay import compare, fixture, read_import, reference_candidate


def sample(time, blocks, received, instance="node@start", best=None):
    return Sample(time, blocks, 200, 0.4, True, str(blocks) if best is None else best, received, instance)


def trials(*rates):
    return [{"blocks_per_second": rate} for rate in rates]


class LiveBenchTests(unittest.TestCase):
    def test_rates_follow_actual_elapsed_time_and_report_serializes(self):
        samples = [sample(2, 100, MIB), sample(4, 108, 3 * MIB), sample(12, 120, 11 * MIB)]
        report = json.loads(json.dumps(live_report("mainnet", samples), allow_nan=False))
        self.assertEqual(report["schema_version"], 2)
        self.assertEqual(report["chain"], "mainnet")
        self.assertEqual(report["summary"], {
            "duration_seconds": 10, "from_height": 100, "to_height": 120, "issue": None,
            "blocks_per_second": 2, "received_mib_per_second": 1,
        })
        self.assertEqual(report["intervals"][0]["blocks_per_second"], 4)
        self.assertEqual(report["samples"][-1]["received_bytes"], 11 * MIB)
        self.assertEqual([row["instance"] for row in report["samples"]], [1, 1, 1])

    def test_stationary_chain_is_a_valid_measurement(self):
        report = live_report("regtest", [sample(0, 12, 0), sample(10, 12, 0)])
        self.assertIsNone(report["summary"]["issue"])
        self.assertEqual(report["summary"]["blocks_per_second"], 0)

    def test_discontinuities_invalidate_the_summary_even_after_recovery(self):
        first = sample(0, 100, 2 * MIB)
        recovered = sample(30, 110, 4 * MIB)
        cases = [
            (sample(10, 96, 3 * MIB), "chain_reorganized"),
            (sample(10, 100, 3 * MIB, best="different"), "chain_reorganized"),
            (sample(10, 105, MIB), "network_counter_reset"),
            (sample(10, 105, 3 * MIB, instance="node@restart"), "node_restarted"),
            (sample(0, 105, 3 * MIB), "invalid_elapsed_time"),
        ]
        for broken, issue in cases:
            with self.subTest(issue=issue, sample=broken):
                report = live_report("mainnet", [first, broken, recovered])
                self.assertEqual(report["intervals"][0]["issue"], issue)
                self.assertEqual(report["summary"]["issue"], issue)
                self.assertIsNone(report["summary"]["received_mib_per_second"])
                self.assertIsNone(report["summary"]["blocks_per_second"])


class ReplayBenchTests(unittest.TestCase):
    def test_median_dispersion_and_regression_threshold(self):
        baseline = trials(100, 101, 99)
        result = compare(baseline, trials(95, 96, 88), 5)
        self.assertEqual(result["baseline"]["median_absolute_deviation"], 1)
        self.assertAlmostEqual(result["throughput_change_pct"], -5)
        self.assertFalse(result["regression"])
        self.assertTrue(compare(baseline, trials(94, 96, 88), 5)["regression"])
        self.assertFalse(compare(baseline, trials(94, 96, 88), None)["regression"])

    def test_fixture_is_reproducible(self):
        with TemporaryDirectory() as directory, TemporaryDirectory() as other:
            first, second = Path(directory), Path(other)
            manifest = fixture(first, 65, 256)
            self.assertEqual(manifest, fixture(second, 65, 256))
            self.assertEqual(manifest["checkpoint_height"], 100)
            self.assertEqual(manifest["target_height"], 165)
            for file in ("checkpoint.dat", "blocks.dat"):
                self.assertEqual((first / file).read_bytes(), (second / file).read_bytes())

    def test_reference_requires_same_fixture_resources_and_cache(self):
        manifest = {"target_hash": "fixture"}
        environment = {"cpu_count": 8, "core_version": "30.4"}
        profile = {"cache_mib": 4096, "median_blocks_per_second": 15}
        reference = {"schema_version": 2, "mode": "replay", "fixture": manifest,
                     "environment": environment, "candidate": profile}
        self.assertEqual(reference_candidate(reference, manifest, environment, 4096), profile)
        with self.assertRaises(ValueError):
            reference_candidate(reference, manifest, environment, 450)
        with self.assertRaises(ValueError):
            reference_candidate(reference, manifest, {**environment, "core_version": "31.0"}, 4096)
        with self.assertRaises(ValueError):
            reference_candidate(reference, {"target_hash": "different"}, environment, 4096)
        with self.assertRaises(ValueError):
            reference_candidate({**reference, "schema_version": 1}, manifest, environment, 4096)

    def test_import_checks_progress_and_includes_shutdown_in_measurement(self):
        checkpoint, target = "a" * 64, "b" * 64
        log = f"""2026-01-01T00:00:00.000000Z Loaded best chain: hashBestChain={checkpoint} height=100
2026-01-01T00:00:01.000000Z Importing blocks file /fixture/blocks.dat...
2026-01-01T00:00:02.000000Z UpdateTip: new best={target} height=101 progress=0.01 cache=2.0MiB(3txo)
2026-01-01T00:00:02.000000Z Stopping after block import
2026-01-01T00:00:03.000000Z Shutdown done
"""
        stats = read_import(log, (checkpoint, "100"), (target, "101"))
        self.assertEqual(stats["duration_seconds"], 2)
        self.assertEqual(stats["import_seconds"], 1)
        self.assertEqual(stats["peak_utxo_cache_mib"], 2)
        with self.assertRaises(RuntimeError):
            read_import(log, (checkpoint, "99"), (target, "101"))
        with self.assertRaises(RuntimeError):
            read_import(log, (checkpoint, "100"), (target, "102"))

    @unittest.skipUnless(os.environ.get("SILICON_SYNC_NATIVE_TEST") == "1", "Requires native regtest replay")
    def test_recreate_node_with_larger_cache_and_resume_the_same_tip(self):
        command = [sys.executable, "-B", str(ROOT / "tools/sync_replay.py"), "--blocks", "65",
                   "--outputs-per-block", "256", "--repeats", "1", "--json"]
        result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, timeout=90)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        manifest = report["fixture"]
        self.assertEqual(report["baseline"]["cache_mib"], 450)
        self.assertEqual(report["candidate"]["cache_mib"], 4096)
        for key in ("baseline", "candidate"):
            trial, = report[key]["trials"]
            self.assertEqual(trial["resumed_from_height"], 100)
            self.assertEqual(trial["resumed_from_hash"], manifest["checkpoint_hash"])
            self.assertEqual(trial["final_height"], 165)
            self.assertEqual(trial["final_hash"], manifest["target_hash"])

        # Force a reference sufficiently high to make the exit status independent of timing noise.
        with TemporaryDirectory() as temporary:
            reference = Path(temporary) / "reference.json"
            report["candidate"]["median_blocks_per_second"] = 1_000_000_000
            reference.write_text(json.dumps(report))
            result = subprocess.run(
                [*command, "--reference", str(reference), "--max-regression-pct", "5"],
                cwd=ROOT, text=True, capture_output=True, timeout=90,
            )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertTrue(json.loads(result.stdout)["against_reference"]["regression"])
