"""The release's files once the binaries are built: the archives, the SBOMs
beside them, and SHA256SUMS.

Archives are reproducible: entries sorted, every time the commit's
(SOURCE_DATE_EPOCH), owner root, mode 0755 for the executables and 0644 for
the rest, whatever the build machine's file system says (Windows has no
executable bit, and a Linux archive built there must still carry one).
"""

from __future__ import annotations

import gzip
import hashlib
import io
import re
import tarfile
import time
import zipfile
from dataclasses import dataclass
from pathlib import Path

SUMS = "SHA256SUMS"


@dataclass(frozen=True)
class Entry:
    """A file in an archive: its name inside the top directory, and its
    content, from a file or given (a document as committed, not as the
    working tree's line endings have it)."""

    name: str
    source: Path | None = None
    executable: bool = False
    data: bytes | None = None

    def content(self) -> bytes:
        if self.data is not None:
            return self.data
        if self.source is None:
            raise ValueError(f"{self.name}: no content")
        return self.source.read_bytes()


def _members(top: str, entries: list[Entry]) -> list[tuple[str, Entry]]:
    names = [entry.name for entry in entries]
    if len(set(names)) != len(names):
        raise ValueError(f"an archive entry is named twice: {sorted(names)}")
    return sorted(((f"{top}/{entry.name}", entry) for entry in entries), key=lambda item: item[0])


def write_tar_gz(path: Path, top: str, entries: list[Entry], epoch: int) -> None:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as archive:
        directories = sorted({f"{top}/{entry.name}".rsplit("/", 1)[0] for entry in entries})
        for directory in directories:
            info = tarfile.TarInfo(directory)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.mtime = epoch
            archive.addfile(info)
        for name, entry in _members(top, entries):
            data = entry.content()
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o755 if entry.executable else 0o644
            info.mtime = epoch
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            archive.addfile(info, io.BytesIO(data))
    with path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as out:
        out.write(buffer.getvalue())


def write_zip(path: Path, top: str, entries: list[Entry], epoch: int) -> None:
    # ZIP times are local-free DOS times from 1980; the commit's UTC time.
    stamp = time.gmtime(max(epoch, 315532800))[:6]
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, entry in _members(top, entries):
            info = zipfile.ZipInfo(name, date_time=stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3  # Unix, so that the mode below is read
            info.external_attr = (0o100755 if entry.executable else 0o100644) << 16
            archive.writestr(info, entry.content())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write_sums(directory: Path, names: list[str]) -> Path:
    """`sha256sum` format (hash, two spaces, name), sorted by name, so that
    `sha256sum -c SHA256SUMS` checks it on Linux."""
    if SUMS in names:
        raise ValueError(f"{SUMS} cannot list itself")
    lines = [f"{sha256(directory / name)}  {name}\n" for name in sorted(names)]
    path = directory / SUMS
    path.write_text("".join(lines), encoding="ascii", newline="\n")
    return path


def verify_sums(directory: Path) -> list[str]:
    """The names SHA256SUMS lists, each checked; raises ValueError on a mismatch."""
    names = []
    for line in (directory / SUMS).read_text(encoding="ascii").splitlines():
        digest, separator, name = line.partition("  ")
        if not separator or len(digest) != 64:
            raise ValueError(f"malformed line in {SUMS}: {line!r}")
        if sha256(directory / name) != digest:
            raise ValueError(f"{name}: checksum mismatch")
        names.append(name)
    return names


def normalize_sbom(text: str, root: Path) -> str:
    """The workspace's own components are `path+file:///<checkout>/...`: the
    build machine's directory says nothing about the release and differs
    between two builds of one commit. It becomes `path+file:///oxrtc/...`."""
    # `/home/runner/work/oxrtc` on Linux, `K:/src/oxrtc` on Windows, where
    # the drive letter's case and a percent-encoded colon both occur.
    prefix = root.resolve().as_posix().strip("/")
    pattern = re.escape(prefix).replace(":", "(?::|%3A)")
    return re.sub(f"file:///{pattern}(?=[/#])", "file:///oxrtc", text, flags=re.IGNORECASE)
