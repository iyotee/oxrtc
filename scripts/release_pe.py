"""What a Windows release binary asks of the system that runs it, read from its
PE headers: the machine and the DLLs it imports (directly or delay-loaded).

release_local.py refuses a binary that needs the Visual C++ runtime
(`VCRUNTIME140.dll`, `MSVCP140.dll`) or the Universal CRT's DLLs
(`api-ms-win-crt-*.dll`, `ucrtbase.dll`): a fresh Windows does not have the
first, and the archive is unzipped and run, not installed. The binary is built
with the C runtime linked in (`+crt-static`) instead. Only PE32+ is read: the
one Windows target is x86-64.
"""

from __future__ import annotations

import re
import struct
from dataclasses import dataclass, field
from pathlib import Path

IMAGE_FILE_MACHINE_AMD64 = 0x8664

_PE32_PLUS = 0x20B
_IMPORT_DIRECTORY = 1
_DELAY_IMPORT_DIRECTORY = 13
# Bounds against a malformed file: a release binary has a few dozen of each.
_MAX_ENTRIES = 4096
_MAX_NAME = 256
# The C runtime a binary must not leave to the system.
_C_RUNTIME = re.compile(r"(vcruntime\d.*|msvcp\d.*|ucrtbase|api-ms-win-crt-.*)\.dll", re.IGNORECASE)


class PeError(ValueError):
    """The file is not a PE32+ image this module reads."""


@dataclass
class PeInfo:
    machine: int
    imports: list[str] = field(default_factory=list)

    def c_runtime(self) -> list[str]:
        """The imported DLLs that are the C runtime, sorted."""
        return sorted(name for name in self.imports if _C_RUNTIME.fullmatch(name))


def _unpack(fmt: str, data: bytes, offset: int) -> tuple:
    size = struct.calcsize(fmt)
    if offset < 0 or offset + size > len(data):
        raise PeError(f"truncated at offset {offset}")
    return struct.unpack_from(fmt, data, offset)


def parse(data: bytes) -> PeInfo:
    """Read the fields above; raises PeError on anything malformed."""
    if data[:2] != b"MZ":
        raise PeError("not an MZ file")
    (pe,) = _unpack("<I", data, 0x3C)
    if data[pe : pe + 4] != b"PE\0\0":
        raise PeError("no PE signature")
    machine, sections_count, _time, _symbols, _count, optional_size, _flags = _unpack("<HHIIIHH", data, pe + 4)
    optional = pe + 24
    (magic,) = _unpack("<H", data, optional)
    if magic != _PE32_PLUS:
        raise PeError(f"not PE32+ (magic {magic:#x})")
    (directories_count,) = _unpack("<I", data, optional + 108)

    def directory(index: int) -> tuple[int, int]:
        if index >= directories_count:
            return (0, 0)
        return _unpack("<II", data, optional + 112 + index * 8)

    sections = []
    table = optional + optional_size
    for i in range(min(sections_count, _MAX_ENTRIES)):
        _name, virtual_size, address, raw_size, raw_pointer = _unpack("<8sIIII", data, table + i * 40)
        sections.append((address, max(virtual_size, raw_size), raw_pointer, raw_size))

    def offset(rva: int) -> int:
        for address, size, raw_pointer, raw_size in sections:
            if address <= rva < address + size and rva - address < raw_size:
                return raw_pointer + rva - address
        raise PeError(f"address {rva:#x} in no section")

    def name(rva: int) -> str:
        start = offset(rva)
        end = data.find(b"\0", start, start + _MAX_NAME)
        if end < 0:
            raise PeError("unterminated DLL name")
        return data[start:end].decode("ascii", "replace")

    info = PeInfo(machine=machine)
    for index, size, name_at in ((_IMPORT_DIRECTORY, 20, 12), (_DELAY_IMPORT_DIRECTORY, 32, 4)):
        rva, _length = directory(index)
        if rva == 0:
            continue
        start = offset(rva)
        for i in range(_MAX_ENTRIES):
            descriptor = data[start + i * size : start + (i + 1) * size]
            if len(descriptor) < size:
                raise PeError("import table outside the file")
            if not any(descriptor):
                break
            (dll,) = struct.unpack_from("<I", descriptor, name_at)
            info.imports.append(name(dll))
    return info


def read(path: Path) -> PeInfo:
    return parse(path.read_bytes())
