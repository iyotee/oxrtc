"""The workflows' contracts with the tree: what a YAML file lists by hand and
the tree lists elsewhere must be the same list. Nothing here runs a workflow.

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"


def fuzz_matrix() -> list[str]:
    workflow = (WORKFLOWS / "fuzz.yml").read_text(encoding="utf-8")
    match = re.search(r"^\s+target: \[(.*?)\]", workflow, re.MULTILINE | re.DOTALL)
    if match is None:
        raise AssertionError("fuzz.yml has no target matrix")
    return [name.strip() for name in match.group(1).split(",") if name.strip()]


def test_jobs() -> list[str]:
    """The names GitHub gives the entries of ci.yml's test matrix."""
    workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    job = workflow.split("\n  test:\n", 1)[1].split("\n    runs-on:", 1)[0]
    entries = re.findall(r"- os: (\S+)(?:\n\s+container: (\S+))?", job)
    return ["test (" + ", ".join(part for part in entry if part) + ")" for entry in entries]


class Fuzz(unittest.TestCase):
    def test_fuzz_yml_runs_every_target_of_the_fuzz_crate(self):
        # Two targets had been added to fuzz/ and not to the workflow: the job
        # would have run without them, and said nothing.
        on_disk = sorted(path.stem for path in (ROOT / "fuzz/fuzz_targets").glob("*.rs"))
        binaries = sorted(re.findall(r'\[\[bin\]\]\nname = "([^"]+)"', (ROOT / "fuzz/Cargo.toml").read_text(encoding="utf-8")))
        self.assertEqual(binaries, on_disk)
        self.assertEqual(sorted(fuzz_matrix()), on_disk)
        self.assertEqual(len(fuzz_matrix()), len(set(fuzz_matrix())))

    def test_every_target_has_the_seeds_the_job_starts_from(self):
        for target in fuzz_matrix():
            seeds = ROOT / "fuzz/seeds" / target
            self.assertTrue(seeds.is_dir() and any(seeds.iterdir()), f"no seed for {target}")


class Tests(unittest.TestCase):
    def test_the_test_job_runs_on_the_three_ffmpeg_lines(self):
        self.assertEqual(test_jobs(), ["test (ubuntu-24.04)", "test (ubuntu-24.04, debian:trixie)", "test (windows-2025)"])

    def test_compatibility_md_names_the_job_of_each_ffmpeg_line(self):
        # D32: an FFmpeg line is supported when a job runs the suite with it.
        document = (ROOT / "docs/COMPATIBILITY.md").read_text(encoding="utf-8")
        section = document.split("\n## FFmpeg\n", 1)[1].split("\n## ", 1)[0]
        rows = [line for line in section.splitlines() if line.startswith("| ") and "`test (" in line]
        self.assertEqual(sorted(re.findall(r"`(test \([^`]+\))`", "\n".join(rows))), sorted(test_jobs()))

    def test_the_skipped_tests_cannot_turn_a_job_green(self):
        # The tests that need FFmpeg or go2rtc pass with a SKIP line when the
        # binary is absent: the job checks the three paths before it runs them.
        workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        check = workflow.split("The binaries are where the tests look", 1)[1].split("cargo test", 1)[0]
        for variable in ("OXRTC_FFMPEG", "OXRTC_FFPROBE", "OXRTC_GO2RTC"):
            self.assertIn(f'"${variable}"', check)


if __name__ == "__main__":
    unittest.main()
