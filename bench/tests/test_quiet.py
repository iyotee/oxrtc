"""The quiet-machine hook: where it is read from, and that a run is always released.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

BENCH = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH))

import quiet  # noqa: E402

# A hook that appends "<verb> <who>" to the file named by its first argument, and exits
# with the code named by its second (0 when absent).
RECORDER = """import sys
log, code = sys.argv[1], int(sys.argv[2])
verb, who = sys.argv[3], sys.argv[4]
with open(log, "a", encoding="utf-8") as f:
    f.write(f"{verb} {who}\\n")
sys.exit(code)
"""


class Hook(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="quiet-"))
        self.script = self.dir / "hook.py"
        self.script.write_text(RECORDER, encoding="utf-8")
        self.log = self.dir / "calls.txt"
        self.said = []

    def out(self, text, **_):
        self.said.append(text)

    def hook(self, code=0):
        return [sys.executable, str(self.script), str(self.log), str(code)]

    def calls(self):
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_no_hook_means_no_call_and_no_wait(self):
        with quiet.quiet_machine("s/r1", cmd=[], out=self.out) as waited:
            pass
        self.assertEqual(waited, 0.0)
        self.assertEqual(self.said, [])

    def test_the_run_is_acquired_then_released(self):
        with quiet.quiet_machine("s/r1", cmd=self.hook(), out=self.out):
            self.assertEqual(self.calls(), ["acquire s/r1"])
        self.assertEqual(self.calls(), ["acquire s/r1", "release s/r1"])
        self.assertIn("held after", self.said[0])

    def test_a_run_that_raises_is_released_all_the_same(self):
        with self.assertRaises(RuntimeError):
            with quiet.quiet_machine("s/r2", cmd=self.hook(), out=self.out):
                raise RuntimeError("the run failed")
        self.assertEqual(self.calls(), ["acquire s/r2", "release s/r2"])

    def test_a_failing_hook_is_said_and_the_run_goes_on(self):
        ran = []
        with quiet.quiet_machine("s/r3", cmd=self.hook(code=3), out=self.out):
            ran.append(True)
        self.assertEqual(ran, [True])
        self.assertIn("acquire exited 3", self.said[0])
        self.assertIn("release exited 3", self.said[1])

    def test_a_hook_that_cannot_start_is_said_and_the_run_goes_on(self):
        ran = []
        with quiet.quiet_machine("s/r4", cmd=[str(self.dir / "no-such-program.exe")], out=self.out):
            ran.append(True)
        self.assertEqual(ran, [True])
        self.assertIn("acquire could not run", self.said[0])


class Where(unittest.TestCase):
    def setUp(self):
        self.file = Path(tempfile.mkdtemp(prefix="quiet-")) / ".bench-hook"

    def test_nothing_configured(self):
        self.assertEqual(quiet.hook_command(env={}, hook_file=self.file), [])

    def test_the_file_first_line_that_is_not_a_comment(self):
        self.file.write_text('# the lanes\' lock\n\n"C:/Program Files/Git/bin/bash.exe" K:/x/lock.sh\n', encoding="utf-8")
        self.assertEqual(quiet.hook_command(env={}, hook_file=self.file),
                         ["C:/Program Files/Git/bin/bash.exe", "K:/x/lock.sh"])

    def test_the_variable_wins_over_the_file(self):
        self.file.write_text("from-the-file\n", encoding="utf-8")
        self.assertEqual(quiet.hook_command(env={"OXRTC_BENCH_HOOK": "from-the-variable --x"}, hook_file=self.file),
                         ["from-the-variable", "--x"])


if __name__ == "__main__":
    unittest.main()
