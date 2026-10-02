import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("watch_web", Path(__file__).with_name("watch-web.py"))
viewer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(viewer)


class SnapshotTests(unittest.TestCase):
    def test_live_holder_overrides_provisional_abandoned_and_preserves_missing_usage(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            run = root / "station/eval/runs/run"
            run.mkdir(parents=True)
            report = {"report": {"run": {"run_id": "run", "created_at": "2026-01-01"},
                "cells": [{"cell_id": "engineer", "slots": [
                    {"case_id": "crew", "trial_index": 0, "class": "abandoned"},
                    {"case_id": "crew", "trial_index": 1, "class": "pass", "latest": {"trial_id": "done"}}]}]},
                "trials": {"done": {"live": {"input_tokens": 42, "tool_calls": 5}}}}
            (run / "report.json").write_text(json.dumps(report))
            progress = {"slots": {"active": {"cell_id": "engineer", "case_id": "crew", "trial_index": 0,
                "pid": os.getpid(), "written_at": "2000-01-01T00:00:00Z", "stage_id": "setup",
                "live": {"input_tokens": None, "tool_calls": 3}}}}
            (run / "progress.json").write_text(json.dumps(progress))
            snapshots = viewer.Snapshots(root)
            index = snapshots.runs()[0]
            self.assertEqual(index["counts"], {"running": 1, "pass": 1})
            detail = snapshots.runs(index["key"])[0]
            self.assertEqual(detail["slots"][0]["state"], "running")
            self.assertIsNone(detail["slots"][0]["live"]["input_tokens"])
            self.assertEqual(detail["slots"][1]["live"]["input_tokens"], 42)
            (run / "progress.json").write_text("{")
            self.assertEqual(snapshots.runs()[0]["counts"], index["counts"])
            progress["slots"]["active"]["pid"] = -1
            (run / "progress.json").write_text(json.dumps(progress))
            self.assertEqual(snapshots.runs()[0]["counts"], {"stopped": 1, "pass": 1})
            stopped = snapshots.runs(index["key"])[0]["slots"][0]
            self.assertEqual(stopped["live"]["tool_calls"], 3)
            self.assertEqual(stopped["stage"], "setup")
            self.assertEqual(snapshots.runs("not-a-run"), [])

    def test_discovery_does_not_follow_homes_outside_the_selected_root(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as outside:
            run = Path(outside) / "eval/runs/private"
            run.mkdir(parents=True)
            (run / "report.json").write_text(json.dumps({"report": {"run": {"run_id": "private"}}}))
            (Path(root) / "linked-home").symlink_to(outside, target_is_directory=True)
            self.assertEqual(viewer.Snapshots(Path(root)).runs(), [])



class ComparisonTests(unittest.TestCase):
    def test_missing_usage_and_stopped_trials_do_not_become_complete_samples(self):
        def measured(value):
            return {"checks": [{"check": "crew_spec_match", "raw": {"satisfied": value, "total": 100}}]}
        summary = viewer.comparison_summary([{"slots": [
            {"state": "pass", "live": {"input_tokens": 100, "output_tokens": 10, "tool_calls": 8,
                "failed_tool_calls": 1, "stages": {"setup": measured(90), "repair": measured(100)}}},
            {"state": "fail", "live": {"input_tokens": None, "reported_input_tokens": 50,
                "reported_output_tokens": 5, "tool_calls": 2, "failed_tool_calls": 1,
                "stages": {"setup": measured(95), "repair": measured(90)}}},
            {"state": "stopped", "live": {"input_tokens": 40, "output_tokens": 4}},
        ]}])
        self.assertEqual(summary["counts"], {"pass": 1, "fail": 1, "stopped": 1})
        self.assertEqual(summary["scores"]["setup"], {"mean": 92.5, "measured": 2})
        self.assertEqual(summary["repair"], {"improved": 1, "worse": 1, "unchanged": 0})
        self.assertEqual(summary["tool_calls"], 10)
        self.assertEqual(summary["failed_tool_calls"], 2)
        usage = summary["tokens"]["input_tokens"]
        self.assertEqual(usage["reported"], 190)
        self.assertEqual(usage["complete"], 2)
        self.assertEqual(usage["settled_complete_count"], 1)
        self.assertEqual(usage["settled_complete_mean"], 100)

    def test_unstarted_round_has_no_scores_or_token_samples(self):
        summary = viewer.comparison_summary([])
        self.assertEqual(summary["trials"], 0)
        self.assertIsNone(summary["scores"]["repair"]["mean"])
        self.assertIsNone(summary["tokens"]["output_tokens"]["settled_complete_mean"])


if __name__ == "__main__":
    unittest.main()
