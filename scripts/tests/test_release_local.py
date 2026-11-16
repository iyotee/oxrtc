"""release_local.py: the platforms, the builder chosen for each, zig found
where pip puts it, the checks of a built binary, and the contracts that keep
release.yml, deny.toml and about.toml on the script's terms.

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import json
import os
import re
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_local as release  # noqa: E402
from elf_fixture import make_elf  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
GLIBC_OK = {"libc.so.6": ["GLIBC_2.17", "GLIBC_2.34"], "libm.so.6": ["GLIBC_2.29"]}


def _toml_list(text: str, key: str) -> list[str]:
    match = re.search(rf"^{key} = \[(.*?)\]", text, re.MULTILINE | re.DOTALL)
    if match is None:
        raise AssertionError(f"no {key} list")
    return re.findall(r'"([^"]+)"', re.sub(r"#.*", "", match.group(1)))


class Contracts(unittest.TestCase):
    def test_the_platforms_are_deny_tomls_targets(self):
        targets = _toml_list((ROOT / "deny.toml").read_text(encoding="utf-8"), "targets")
        self.assertEqual(sorted(targets), sorted(platform.target for platform in release.PLATFORMS.values()))

    def test_release_yml_builds_each_platform_through_this_script(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        matrix = dict(re.findall(r"- platform: (\S+)\n\s+os: \S+\n\s+target: (\S+)", workflow))
        self.assertEqual(matrix, {name: platform.target for name, platform in release.PLATFORMS.items()})
        self.assertIn('scripts/release_local.py --platform "${{ matrix.platform }}"', workflow)

    def test_the_tool_versions_are_the_same_here_and_in_release_yml(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        documented = release.__doc__ or ""
        for tool in ("cargo-about", "cargo-cyclonedx"):
            in_workflow = re.findall(rf"{tool}@(\S+)", workflow)
            in_script = re.findall(rf"{tool}@(\S+)", documented)
            self.assertEqual(len(in_workflow), 1, tool)
            self.assertEqual(in_workflow, in_script, tool)

    def test_the_notices_accept_exactly_the_licences_deny_toml_allows(self):
        allowed = _toml_list((ROOT / "deny.toml").read_text(encoding="utf-8"), "allow")
        accepted = _toml_list((ROOT / "scripts/about.toml").read_text(encoding="utf-8"), "accepted")
        self.assertEqual(accepted, allowed)

    def test_the_sboms_cargo_cyclonedx_leaves_in_the_tree_are_ignored(self):
        self.assertIn("/apps/*/*.cdx.json", (ROOT / ".gitignore").read_text(encoding="utf-8").splitlines())


class Builder(unittest.TestCase):
    def test_the_hosts_own_target_builds_with_cargo(self):
        for platform in release.PLATFORMS.values():
            self.assertEqual(release.builder_for(platform, platform.target, "auto"), "cargo")

    def test_linux_from_another_host_builds_with_zigbuild(self):
        for name in ("linux-amd64", "linux-arm64"):
            self.assertEqual(release.builder_for(release.PLATFORMS[name], "x86_64-pc-windows-msvc", "auto"), "zigbuild")
        self.assertEqual(
            release.builder_for(release.PLATFORMS["linux-arm64"], "x86_64-unknown-linux-gnu", "auto"), "zigbuild"
        )

    def test_windows_builds_on_windows_only_and_a_choice_given_is_kept(self):
        with self.assertRaises(RuntimeError):
            release.builder_for(release.PLATFORMS["windows-amd64"], "x86_64-unknown-linux-gnu", "auto")
        self.assertEqual(release.builder_for(release.PLATFORMS["linux-arm64"], "x86_64-unknown-linux-gnu", "cargo"), "cargo")


class Zig(unittest.TestCase):
    def test_a_zig_already_named_or_on_the_path_is_used(self):
        named = {"CARGO_ZIGBUILD_ZIG_PATH": "/opt/zig", "PATH": ""}
        self.assertIs(release.zig_environment(named), named)
        with tempfile.TemporaryDirectory() as directory:
            zig = Path(directory) / ("zig.exe" if os.name == "nt" else "zig")
            zig.write_bytes(b"")
            zig.chmod(0o755)
            on_path = {"PATH": directory, "PATHEXT": ".EXE"}
            self.assertIs(release.zig_environment(on_path), on_path)

    def test_zig_from_pip_is_found_in_its_package(self):
        with tempfile.TemporaryDirectory() as directory:
            zig = Path(directory) / ("zig.exe" if os.name == "nt" else "zig")
            zig.write_bytes(b"")
            package = types.ModuleType("ziglang")
            package.__file__ = str(Path(directory) / "__init__.py")
            with mock.patch.dict(sys.modules, {"ziglang": package}):
                environment = release.zig_environment({"PATH": ""})
            self.assertEqual(environment["CARGO_ZIGBUILD_ZIG_PATH"], str(zig))

    def test_no_zig_anywhere_says_how_to_install_it(self):
        with mock.patch.dict(sys.modules, {"ziglang": None}), self.assertRaises(RuntimeError) as raised:
            release.zig_environment({"PATH": ""})
        self.assertIn("pip install --user ziglang", str(raised.exception))


class CheckBinary(unittest.TestCase):
    def check(self, name: str, data: bytes) -> str:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "oxrtc"
            path.write_bytes(data)
            return release.check_binary(release.PLATFORMS[name], path, "x86_64-pc-windows-msvc")

    def test_a_binary_for_its_platform_passes_and_says_what_was_checked(self):
        line = self.check("linux-arm64", make_elf(183, "/lib/ld-linux-aarch64.so.1",
                                                  ["libm.so.6", "libc.so.6", "ld-linux-aarch64.so.1"], GLIBC_OK))
        self.assertIn("ELF machine 183", line)
        self.assertIn("glibc symbols up to 2.34", line)

    def test_another_architecture_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "machine 62"):
            self.check("linux-arm64", make_elf(62, "/lib/ld-linux-aarch64.so.1", ["libc.so.6"], GLIBC_OK))

    def test_a_glibc_above_the_floor_is_refused(self):
        newer = {"libc.so.6": ["GLIBC_2.17", "GLIBC_2.36"]}
        with self.assertRaisesRegex(RuntimeError, "above the floor"):
            self.check("linux-amd64", make_elf(62, "/lib64/ld-linux-x86-64.so.2", ["libc.so.6"], newer))

    def test_a_library_the_system_may_not_have_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "libssl"):
            self.check("linux-amd64", make_elf(62, "/lib64/ld-linux-x86-64.so.2", ["libc.so.6", "libssl.so.3"], GLIBC_OK))

    def test_another_interpreter_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "interpreter"):
            self.check("linux-amd64", make_elf(62, "/lib/ld-musl-x86_64.so.1", ["libc.so.6"], GLIBC_OK))

    def test_windows_is_not_run_from_another_host(self):
        with tempfile.TemporaryDirectory() as directory:
            line = release.check_binary(release.PLATFORMS["windows-amd64"], Path(directory) / "oxrtc.exe", "x86_64-unknown-linux-gnu")
        self.assertEqual(line, "not run (another host)")


class Command(unittest.TestCase):
    def test_a_dirty_tree_is_refused_before_anything_is_built(self):
        def output(command):
            return " M crates/core/src/lib.rs" if command[:2] == ["git", "status"] else ""

        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(release, "output", side_effect=output), \
                mock.patch.object(release, "build") as build, \
                mock.patch("sys.stderr"):
            self.assertEqual(release.main(["--out", directory]), 1)
        build.assert_not_called()

    def test_the_notices_leave_out_the_members_only_not_a_patched_path_crate(self):
        metadata = {
            "workspace_members": ["path+file:///oxrtc/crates/core#oxrtc-core@0.1.0"],
            "packages": [
                {"name": "oxrtc-core", "id": "path+file:///oxrtc/crates/core#oxrtc-core@0.1.0"},
                {"name": "h264-reader", "id": "path+file:///oxrtc/third_party/h264-reader#0.9.0"},
            ],
        }
        with mock.patch.object(release, "output", return_value=json.dumps(metadata)) as output:
            self.assertEqual(release.workspace_members(), {"oxrtc-core"})
        self.assertIn("--no-deps", output.call_args.args[0])

    def test_the_shipped_binaries_are_oxrtc_and_on_linux_its_wrapper(self):
        self.assertEqual(release.PLATFORMS["windows-amd64"].binaries, ("oxrtc",))
        self.assertEqual(release.PLATFORMS["windows-amd64"].executable("oxrtc"), "oxrtc.exe")
        for name in ("linux-amd64", "linux-arm64"):
            self.assertEqual(release.PLATFORMS[name].binaries, ("oxrtc", "oxrtc-exec"))


if __name__ == "__main__":
    unittest.main()
