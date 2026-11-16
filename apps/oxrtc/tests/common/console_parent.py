"""Plays openIVR's launcher for the Ctrl+Break tests (apps/oxrtc/tests/ctrl_break.rs).

Windows only. A console event can only be sent from a process that shares the
target's console, and without unsafe code the Rust test cannot send one, so
this script does what openIVR does (`run_all.py`, `ask_to_stop`): start the
program in a console group of its own (CREATE_NEW_PROCESS_GROUP, as
`app/services/streaming/go2rtc_manager.py` does for the media server) and
send CTRL_BREAK_EVENT to the group's id.

    console_parent.py <oxrtc.exe> <config> own|other

Everything is said on stdout, one line each: `PID <n>` once oxrtc is started,
`LOG <line>` for each line oxrtc writes. The test answers on stdin with a line.
After the first one the break goes to oxrtc's own group (`own`, then
`EXIT <code> <seconds>` when it has ended) or to a bystander's group (`other`,
then `BYSTANDER <code>`, and oxrtc is left running until a second line comes).
"""

import signal
import subprocess
import sys
import threading
import time

exe, config, mode = sys.argv[1:4]
GROUP = subprocess.CREATE_NEW_PROCESS_GROUP


def say(*parts):
    print(*parts, flush=True)


oxrtc = subprocess.Popen(
    [exe, "-config", config],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    creationflags=GROUP,
    text=True,
    encoding="utf-8",
    errors="replace",
)


def relay():
    for line in oxrtc.stdout:
        say("LOG", line.rstrip("\r\n"))


reader = threading.Thread(target=relay, daemon=True)
reader.start()
bystander = None
try:
    if mode == "other":
        bystander = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(120)"], creationflags=GROUP
        )
    say("PID", oxrtc.pid)
    sys.stdin.readline()
    started = time.perf_counter()
    if mode == "own":
        oxrtc.send_signal(signal.CTRL_BREAK_EVENT)
        try:
            code = oxrtc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            code = "running"
        reader.join(timeout=2)
        say("EXIT", code, round(time.perf_counter() - started, 3))
    else:
        bystander.send_signal(signal.CTRL_BREAK_EVENT)
        try:
            code = bystander.wait(timeout=10)
        except subprocess.TimeoutExpired:
            code = "running"
        say("BYSTANDER", code)
        sys.stdin.readline()
finally:
    for proc in (oxrtc, bystander):
        if proc is not None and proc.poll() is None:
            proc.kill()
