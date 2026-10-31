#!/usr/bin/env python3
"""Execute the workflow identity guard against disposable Git histories."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]

class CandidateIdentity(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for package in ("dinero-sv2-miner", "dinero-sv2-gpu-miner"):
            path = self.root / "crates" / package / "Cargo.toml"
            path.parent.mkdir(parents=True)
            path.write_text('[package]\nversion = "0.2.13"\n')
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.DEVNULL, text=True).strip()
        git("init", "-q")
        git("add", ".")
        git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        self.head = git("rev-parse", "HEAD")
        workflow = yaml.load((ROOT / ".github/workflows/miner-release.yml").read_text(), Loader=yaml.BaseLoader)
        self.guard = next(step["run"] for step in workflow["jobs"]["build"]["steps"]
                          if step.get("name") == "Require immutable source and version identity")

    def run_guard(self, event="workflow_dispatch", source=None, ref_type="branch", ref_name="candidate"):
        env = dict(os.environ, GITHUB_EVENT_NAME=event, GITHUB_REF_TYPE=ref_type,
                   GITHUB_REF_NAME=ref_name, CANDIDATE_SOURCE=self.head if source is None else source)
        return subprocess.run(["bash", "-euo", "pipefail", "-c", self.guard],
                              cwd=self.root, env=env, capture_output=True, text=True)

    def test_exact_candidate_succeeds(self):
        result = self.run_guard()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_and_mutable_source_fail(self):
        for value in ("", "main", self.head[:12], "../main"):
            with self.subTest(value=value):
                self.assertNotEqual(self.run_guard(source=value).returncode, 0)

    def test_different_full_sha_fails(self):
        self.assertNotEqual(self.run_guard(source="0" * 40).returncode, 0)

    def test_manual_tag_ref_fails(self):
        self.assertNotEqual(self.run_guard(ref_type="tag", ref_name="miner-v0.2.13").returncode, 0)

    def test_mixed_cpu_gpu_versions_fail(self):
        path = self.root / "crates/dinero-sv2-gpu-miner/Cargo.toml"
        path.write_text('[package]\nversion = "0.2.12"\n')
        self.assertNotEqual(self.run_guard().returncode, 0)

    def test_release_version_match_and_mismatch(self):
        self.assertEqual(self.run_guard("push", "", "tag", "miner-v0.2.13").returncode, 0)
        self.assertNotEqual(self.run_guard("push", "", "tag", "miner-v0.2.12").returncode, 0)
        self.assertNotEqual(self.run_guard("push", "", "branch", "main").returncode, 0)

    def test_unexpected_event_fails(self):
        self.assertNotEqual(self.run_guard(event="pull_request").returncode, 0)

if __name__ == "__main__":
    unittest.main()
