"""Build oxrtc's release assets: linux-amd64, linux-arm64 (Raspberry Pi 5)
and windows-amd64 archives, a CycloneDX SBOM per binary, the third-party
notices in each archive (R16), and SHA256SUMS.

It runs on a developer's machine (GitHub Actions minutes are not required)
and in .github/workflows/release.yml, one platform per job, so that both
produce the same files. From Windows the Linux targets are cross-compiled with
cargo-zigbuild, zig serving as linker and as the C compiler aws-lc-sys needs
(R15); nothing needs administrator rights or a system-wide toolchain:

    python -m pip install --user ziglang==0.17.1
    cargo install --locked cargo-zigbuild@0.24.0
    cargo install --locked cargo-about@0.9.3 --features cli
    cargo install --locked cargo-cyclonedx@0.5.10

    python scripts/release_local.py --out <directory>       # all three
    python scripts/release_local.py --platform linux-arm64 --out <directory>

The tree must be clean (the archives name the commit) unless --allow-dirty.
Nothing is tagged or published.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_elf  # noqa: E402
import release_notices  # noqa: E402
import release_pe  # noqa: E402
import release_package  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
# The oldest glibc the Linux binaries run on: Ubuntu 22.04's, so Debian 12 /
# Raspberry Pi OS Bookworm (2.36) and newer too. zigbuild links against it;
# release.yml builds on ubuntu-22.04 for the same floor.
GLIBC_FLOOR = (2, 35)
# What a Linux binary may ask of the system: glibc and its loader, nothing else
# (no OpenSSL, no C++ runtime: aws-lc is linked in).
ALLOWED_LIBRARIES = {"libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1", "libgcc_s.so.1"}


@dataclass(frozen=True)
class Platform:
    name: str
    target: str
    # The binaries shipped: oxrtc-exec only matters on Linux (R13, R29).
    binaries: tuple[str, ...]
    elf_machine: int | None = None
    interpreter: str | None = None

    @property
    def windows(self) -> bool:
        return self.target.endswith("-windows-msvc")

    def executable(self, binary: str) -> str:
        return f"{binary}.exe" if self.windows else binary


PLATFORMS = {
    platform.name: platform
    for platform in (
        Platform(
            "linux-amd64",
            "x86_64-unknown-linux-gnu",
            ("oxrtc", "oxrtc-exec"),
            release_elf.EM_X86_64,
            "/lib64/ld-linux-x86-64.so.2",
        ),
        Platform(
            "linux-arm64",
            "aarch64-unknown-linux-gnu",
            ("oxrtc", "oxrtc-exec"),
            release_elf.EM_AARCH64,
            "/lib/ld-linux-aarch64.so.1",
        ),
        Platform("windows-amd64", "x86_64-pc-windows-msvc", ("oxrtc",)),
    )
}


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+", " ".join(command), flush=True)
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def output(command: list[str]) -> str:
    return subprocess.run(command, cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()


def committed(name: str) -> bytes:
    """A file as HEAD has it: LF line endings whatever core.autocrlf does."""
    return subprocess.run(["git", "show", f"HEAD:{name}"], cwd=ROOT, check=True, capture_output=True).stdout


def host_target() -> str:
    for line in output(["rustc", "-vV"]).splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise RuntimeError("rustc -vV names no host")


def builder_for(platform: Platform, host: str, choice: str) -> str:
    """`cargo` for the host's own target, `zigbuild` to cross-compile Linux."""
    if choice != "auto":
        return choice
    if platform.target == host:
        return "cargo"
    if not platform.windows:
        return "zigbuild"
    raise RuntimeError(f"{platform.name} builds on Windows only (MSVC); this host is {host}")


def zig_environment(environ: dict[str, str]) -> dict[str, str]:
    """cargo-zigbuild finds `zig` on the PATH or in $CARGO_ZIGBUILD_ZIG_PATH;
    zig from pip (`ziglang`) is neither, so its path is set here."""
    if environ.get("CARGO_ZIGBUILD_ZIG_PATH") or shutil.which("zig", path=environ.get("PATH")):
        return environ
    try:
        import ziglang  # type: ignore[import-not-found]
    except ImportError as error:
        raise RuntimeError("zig not found: python -m pip install --user ziglang==0.17.1") from error
    directory = Path(ziglang.__file__).parent
    zig = directory / ("zig.exe" if os.name == "nt" else "zig")
    if not zig.is_file():
        raise RuntimeError(f"the ziglang package has no {zig.name} in {directory}")
    return {**environ, "CARGO_ZIGBUILD_ZIG_PATH": str(zig)}


def build_environment(platform: Platform, environ: dict[str, str]) -> dict[str, str]:
    """On Windows the C runtime is linked in (`+crt-static`): the archive is
    unzipped and run, and a fresh Windows has no VCRUNTIME140.dll. Set for the
    target only, so build scripts and `cargo test` are untouched; a RUSTFLAGS
    of the caller's would override it, and check_binary would then refuse."""
    if not platform.windows:
        return environ
    variable = "CARGO_TARGET_" + platform.target.upper().replace("-", "_") + "_RUSTFLAGS"
    return {**environ, variable: " ".join(filter(None, (environ.get(variable), "-C target-feature=+crt-static")))}


def build(platform: Platform, builder: str) -> Path:
    packages = [arg for binary in platform.binaries for arg in ("-p", binary)]
    if builder == "zigbuild":
        target = f"{platform.target}.{GLIBC_FLOOR[0]}.{GLIBC_FLOOR[1]}"
        run(["cargo", "zigbuild", "--release", "--locked", *packages, "--target", target], env=zig_environment(dict(os.environ)))
    else:
        run(["cargo", "build", "--release", "--locked", *packages, "--target", platform.target],
            env=build_environment(platform, dict(os.environ)))
    return ROOT / "target" / platform.target / "release"


def check_windows(path: Path, host: str, target: str) -> str:
    info = release_pe.read(path)
    if info.machine != release_pe.IMAGE_FILE_MACHINE_AMD64:
        raise RuntimeError(f"{path}: PE machine {info.machine:#x}, expected {release_pe.IMAGE_FILE_MACHINE_AMD64:#x}")
    runtime = info.c_runtime()
    if runtime:
        raise RuntimeError(f"{path}: needs the C runtime from the system ({', '.join(runtime)}): build with +crt-static")
    checked = f"PE machine {info.machine:#x}, imports {' '.join(sorted(info.imports, key=str.lower))}"
    if host != target:
        return f"{checked}, not run (another host)"
    printed = output([str(path), "--version"])
    if not printed.startswith("oxrtc "):
        raise RuntimeError(f"{path.name} --version printed {printed!r}")
    return f"{checked}, runs: {printed}"


def check_binary(platform: Platform, path: Path, host: str) -> str:
    """What the binary was checked for, in one line; raises on a mismatch."""
    if platform.windows:
        return check_windows(path, host, platform.target)
    info = release_elf.read(path)
    if info.machine != platform.elf_machine:
        raise RuntimeError(f"{path}: ELF machine {info.machine}, expected {platform.elf_machine}")
    if info.interpreter != platform.interpreter:
        raise RuntimeError(f"{path}: interpreter {info.interpreter}, expected {platform.interpreter}")
    extra = set(info.needed) - ALLOWED_LIBRARIES - {Path(platform.interpreter or "").name}
    if extra:
        raise RuntimeError(f"{path}: needs {sorted(extra)}")
    newest = info.max_glibc()
    if newest is not None and newest > GLIBC_FLOOR:
        raise RuntimeError(f"{path}: needs glibc {newest}, above the floor {GLIBC_FLOOR}")
    shown = ".".join(map(str, newest)) if newest else "none"
    return f"ELF machine {info.machine}, {info.interpreter}, needs {' '.join(info.needed)}, glibc symbols up to {shown}"


def sboms(platform: Platform, epoch: int) -> dict[str, str]:
    """One CycloneDX SBOM per shipped binary (cargo-cyclonedx writes one per
    workspace binary beside its manifest; they are read, then removed)."""
    written = {binary: ROOT / "apps" / binary / f"{binary}_bin.cdx.json" for binary in PLATFORMS["linux-amd64"].binaries}
    env = {**os.environ, "SOURCE_DATE_EPOCH": str(epoch)}
    try:
        run(
            ["cargo", "cyclonedx", "--manifest-path", "apps/oxrtc/Cargo.toml", "--format", "json", "--spec-version", "1.5",
             "--describe", "binaries", "--target", platform.target, "-q"],
            env=env,
        )
        return {binary: release_package.normalize_sbom(written[binary].read_text(encoding="utf-8"), ROOT) for binary in platform.binaries}
    finally:
        for path in written.values():
            path.unlink(missing_ok=True)


def workspace_members() -> set[str]:
    """The workspace's own crates by name: what the notices leave out. A path
    crate outside them (third_party/, patched in) is third-party."""
    metadata = json.loads(output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", "--offline"]))
    members = set(metadata["workspace_members"])
    return {package["name"] for package in metadata["packages"] if package["id"] in members}


def notices(platform: Platform, title: str, scratch: Path) -> str:
    reports = []
    for binary in platform.binaries:
        report = scratch / f"about-{binary}.json"
        run(["cargo", "about", "generate", "--format", "json", "-c", "scripts/about.toml", "--offline", "--locked", "--fail",
             "-m", f"apps/{binary}/Cargo.toml", "--target", platform.target, "-o", str(report)])
        reports.append(report)
    return release_notices.render_files(reports, title, workspace_members())


def assemble(platform: Platform, version: str, binaries: Path, out: Path, epoch: int, host: str) -> list[str]:
    """The archive and its SBOMs for one platform; returns the files written."""
    top = f"oxrtc-{version}-{platform.name}"
    for binary in platform.binaries:
        print(f"  {platform.executable(binary)}: {check_binary(platform, binaries / platform.executable(binary), host)}", flush=True)
    with tempfile.TemporaryDirectory(prefix="oxrtc-release-") as scratch_name:
        scratch = Path(scratch_name)
        notice_file = scratch / "THIRD_PARTY_NOTICES.txt"
        notice_file.write_text(notices(platform, f"Third-party notices: oxrtc {version}, {platform.name}", scratch), encoding="utf-8", newline="\n")
        entries = [release_package.Entry(platform.executable(binary), binaries / platform.executable(binary), executable=True)
                   for binary in platform.binaries]
        entries += [release_package.Entry(name, data=committed(name)) for name in ("LICENSE-MIT", "LICENSE-APACHE", "README.md")]
        entries.append(release_package.Entry("THIRD_PARTY_NOTICES.txt", notice_file))
        written = []
        for binary, sbom in sboms(platform, epoch).items():
            name = f"{binary}-{version}-{platform.name}.cdx.json"
            (out / name).write_text(sbom, encoding="utf-8", newline="\n")
            entries.append(release_package.Entry(f"sbom/{binary}.cdx.json", out / name))
            written.append(name)
        archive = f"{top}.zip" if platform.windows else f"{top}.tar.gz"
        writer = release_package.write_zip if platform.windows else release_package.write_tar_gz
        writer(out / archive, top, entries, epoch)
        written.append(archive)
    return written


def default_version(dirty: bool) -> str:
    version = next(line.split('"')[1] for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines()
                   if line.startswith("version = "))
    commit = output(["git", "rev-parse", "--short=7", "HEAD"])
    return f"v{version}-{commit}{'-dirty' if dirty else ''}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--out", type=Path, required=True, help="directory for the assets (created)")
    parser.add_argument("--platform", action="append", choices=sorted(PLATFORMS), help="repeatable; default: all three")
    parser.add_argument("--version", help="the assets' version, e.g. v0.1.0 (default: v<workspace version>-<commit>)")
    parser.add_argument("--builder", choices=("auto", "cargo", "zigbuild"), default="auto",
                        help="cargo for a native or a configured cross build, zigbuild for Linux from elsewhere")
    parser.add_argument("--no-sums", action="store_true", help="leave SHA256SUMS to the caller (release.yml's publish job)")
    parser.add_argument("--allow-dirty", action="store_true", help="build from a tree with uncommitted changes")
    args = parser.parse_args(argv)

    dirty = bool(output(["git", "status", "--porcelain", "--untracked-files=no"]))
    if dirty and not args.allow_dirty:
        print("the tree has uncommitted changes; commit them or pass --allow-dirty", file=sys.stderr)
        return 1
    version = args.version or default_version(dirty)
    epoch = int(output(["git", "log", "-1", "--format=%ct"]))
    host = host_target()
    args.out.mkdir(parents=True, exist_ok=True)
    out = args.out.resolve()

    written: list[str] = []
    for name in args.platform or list(PLATFORMS):
        platform = PLATFORMS[name]
        builder = builder_for(platform, host, args.builder)
        print(f"== {platform.name} ({platform.target}, {builder})", flush=True)
        written += assemble(platform, version, build(platform, builder), out, epoch, host)
    if not args.no_sums:
        release_package.write_sums(out, written)
        release_package.verify_sums(out)
    for name in written + ([] if args.no_sums else [release_package.SUMS]):
        print(f"  {out / name}  {(out / name).stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
