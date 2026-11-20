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
        self.assertEqual(
            test_jobs(),
            [
                "test (ubuntu-24.04)",
                "test (ubuntu-24.04, debian:trixie)",
                "test (ubuntu-24.04-arm, debian:trixie)",
                "test (windows-2025)",
            ],
        )

    def test_go2rtc_is_checked_against_a_checksum_on_each_linux_architecture(self):
        # The arm64 job downloads another binary: it has a checksum of its
        # own, and no branch of the download leaves the file unchecked.
        workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        step = workflow.split("FFmpeg and go2rtc (Linux)", 1)[1].split("FFmpeg and go2rtc (Windows)", 1)[0]
        for asset, variable in (
            ("go2rtc_linux_arm64", "GO2RTC_SHA256_LINUX_ARM64"),
            ("go2rtc_linux_amd64", "GO2RTC_SHA256_LINUX_AMD64"),
        ):
            self.assertIn(f'asset={asset}; sum="${variable}"', step)
            self.assertRegex(workflow, rf'{variable}: "[0-9a-f]{{64}}"')
        self.assertIn('echo "${sum}  $RUNNER_TEMP/bin/go2rtc" | sha256sum -c -', step)

    def test_compatibility_md_names_the_job_of_each_ffmpeg_line(self):
        # D32: an FFmpeg line is supported when a job runs the suite with it.
        document = (ROOT / "docs/COMPATIBILITY.md").read_text(encoding="utf-8")
        section = document.split("\n## FFmpeg\n", 1)[1].split("\n## ", 1)[0]
        rows = [line for line in section.splitlines() if line.startswith("| ") and "`test (" in line]
        self.assertEqual(sorted(re.findall(r"`(test \([^`]+\))`", "\n".join(rows))), sorted(test_jobs()))

    def test_the_windows_ffmpeg_install_is_asked_again_when_the_feed_fails(self):
        # Chocolatey's community feed answered 503 and then 504 to this step
        # (runs 44606561602 and 44734441686): the job was red in twenty
        # seconds on a commit that had nothing wrong. Asked three times, and a
        # third failure is still a failure.
        workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        step = workflow.split("FFmpeg and go2rtc (Windows)", 1)[1].split("The binaries are where the tests look", 1)[0]
        attempts = step.split("foreach ($attempt in 1..3) {", 1)
        self.assertEqual(len(attempts), 2, "the install is not in a loop of three attempts")
        loop = attempts[1].split("\n          }\n", 1)[0]
        self.assertIn("choco install ffmpeg --version=8.0.1 -y --no-progress", loop)
        self.assertIn("if ($LASTEXITCODE -eq 0) { break }", loop)
        self.assertIn("if ($attempt -eq 3) { throw", loop)
        self.assertEqual(step.count("choco install ffmpeg --version"), 1, "an install outside the loop")

    def test_the_skipped_tests_cannot_turn_a_job_green(self):
        # The tests that need FFmpeg or go2rtc pass with a SKIP line when the
        # binary is absent: the job checks the three paths before it runs them.
        workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        check = workflow.split("The binaries are where the tests look", 1)[1].split("cargo test", 1)[0]
        for variable in ("OXRTC_FFMPEG", "OXRTC_FFPROBE", "OXRTC_GO2RTC"):
            self.assertIn(f'"${variable}"', check)

    def test_a_skip_is_a_failure_in_ci_and_every_skip_goes_through_the_check(self):
        # A skipped test passes and its SKIP line is not shown: CI sets
        # OXRTC_NO_SKIP, and every place a test skips itself asks `skip`, which
        # fails the test there. The one exception is an ignored test (the
        # owner's cameras), which no job runs.
        workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        self.assertRegex(workflow, r'(?m)^  OXRTC_NO_SKIP: "1"$')
        printed, checked = [], 0
        for directory in ("apps", "crates"):
            for source in sorted((ROOT / directory).rglob("*.rs")):
                text = source.read_text(encoding="utf-8")
                checked += text.count('var_os("OXRTC_NO_SKIP").is_none()')
                for line in text.splitlines():
                    if "SKIP" in line and "eprintln!" in line and 'eprintln!("SKIP: {why}");' not in line:
                        printed.append(f"{source.relative_to(ROOT).as_posix()}: {line.strip()}")
        ignored = 'apps/oxrtc/tests/real_cameras.rs: eprintln!("SKIP: OXRTC_REAL_CONFIG is not set");'
        self.assertEqual(printed, [ignored])
        self.assertEqual(checked, 6, "one `skip` per test harness")
        # The same for the one check a kernel without Landlock leaves out.
        confinement = (ROOT / "apps/oxrtc-exec/tests/linux.rs").read_text(encoding="utf-8")
        branch = confinement.split("Landlock::None => {", 1)[1].split("_ =>", 1)[0]
        self.assertIn('var_os("OXRTC_NO_SKIP").is_some()', branch)
        self.assertIn("assert!(!must", branch)


if __name__ == "__main__":
    unittest.main()
