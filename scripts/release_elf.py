"""What a Linux release binary asks of the system that runs it, read from its
ELF headers: the machine, the program interpreter, the shared libraries and
the highest glibc symbol version.

A cross-built binary cannot be run on the build machine, so this is how
release_local.py checks that it is the architecture it claims to be and that
it starts on the oldest glibc the release promises (2.35: Ubuntu 22.04, and so
Debian 12 / Raspberry Pi OS Bookworm). Only ELF64 little-endian is read: the
two Linux targets are x86-64 and AArch64.
"""

from __future__ import annotations

import re
import struct
from dataclasses import dataclass, field
from pathlib import Path

EM_X86_64 = 62
EM_AARCH64 = 183

_PT_INTERP = 3
_SHT_DYNAMIC = 6
_SHT_GNU_VERNEED = 0x6FFFFFFE
_DT_NULL = 0
_DT_NEEDED = 1
# Bounds against a malformed file: a release binary has a few dozen of each.
_MAX_ENTRIES = 65536


class ElfError(ValueError):
    """The file is not an ELF64 little-endian executable this module reads."""


@dataclass
class ElfInfo:
    machine: int
    interpreter: str | None
    needed: list[str] = field(default_factory=list)
    # Versions required from each library: {"libc.so.6": {"GLIBC_2.34", ...}}.
    versions: dict[str, set[str]] = field(default_factory=dict)

    def max_glibc(self) -> tuple[int, ...] | None:
        """The highest GLIBC_x.y[.z] symbol version required, if any."""
        found = [
            parsed
            for names in self.versions.values()
            for name in names
            if (parsed := glibc_version(name)) is not None
        ]
        return max(found) if found else None


def glibc_version(name: str) -> tuple[int, ...] | None:
    """`GLIBC_2.34` -> (2, 34); anything else (GLIBC_PRIVATE, GCC_3.0) -> None."""
    match = re.fullmatch(r"GLIBC_(\d+(?:\.\d+)+)", name)
    return tuple(int(part) for part in match.group(1).split(".")) if match else None


def _unpack(fmt: str, data: bytes, offset: int) -> tuple:
    size = struct.calcsize(fmt)
    if offset < 0 or offset + size > len(data):
        raise ElfError(f"truncated at offset {offset}")
    return struct.unpack_from(fmt, data, offset)


def _string(data: bytes, table: tuple[int, int], index: int) -> str:
    start, size = table
    if index < 0 or index >= size:
        raise ElfError(f"string index {index} outside its table")
    end = data.find(b"\0", start + index, start + size)
    if end < 0:
        raise ElfError("unterminated string")
    return data[start + index : end].decode("utf-8", "replace")


def parse(data: bytes) -> ElfInfo:
    """Read the fields above; raises ElfError on anything malformed."""
    if data[:4] != b"\x7fELF":
        raise ElfError("not an ELF file")
    if data[4:6] != b"\x02\x01":
        raise ElfError("not ELF64 little-endian")
    (machine,) = _unpack("<H", data, 0x12)
    (phoff, shoff) = _unpack("<QQ", data, 0x20)
    (phentsize, phnum, shentsize, shnum) = _unpack("<HHHH", data, 0x36)

    interpreter = None
    for i in range(min(phnum, _MAX_ENTRIES)):
        p_type, _flags, p_offset, _vaddr, _paddr, p_filesz = _unpack("<IIQQQQ", data, phoff + i * phentsize)
        if p_type == _PT_INTERP:
            if p_offset + p_filesz > len(data):
                raise ElfError("interpreter outside the file")
            interpreter = data[p_offset : p_offset + p_filesz].rstrip(b"\0").decode("utf-8", "replace")

    sections = []
    for i in range(min(shnum, _MAX_ENTRIES)):
        _name, sh_type, _flags, _addr, sh_offset, sh_size, sh_link, sh_info = _unpack(
            "<IIQQQQII", data, shoff + i * shentsize
        )
        sections.append((sh_type, sh_offset, sh_size, sh_link, sh_info))

    def strtab(link: int) -> tuple[int, int]:
        if link >= len(sections):
            raise ElfError(f"section link {link} outside the table")
        _type, offset, size, _link, _info = sections[link]
        if offset + size > len(data):
            raise ElfError("string table outside the file")
        return (offset, size)

    info = ElfInfo(machine=machine, interpreter=interpreter)
    for sh_type, offset, size, link, count in sections:
        if sh_type == _SHT_DYNAMIC:
            strings = strtab(link)
            for i in range(min(size // 16, _MAX_ENTRIES)):
                tag, value = _unpack("<qQ", data, offset + i * 16)
                if tag == _DT_NULL:
                    break
                if tag == _DT_NEEDED:
                    info.needed.append(_string(data, strings, value))
        elif sh_type == _SHT_GNU_VERNEED:
            strings = strtab(link)
            entry = offset
            for _ in range(min(count, _MAX_ENTRIES)):
                _version, aux_count, file_name, aux, next_entry = _unpack("<HHIII", data, entry)
                names = info.versions.setdefault(_string(data, strings, file_name), set())
                aux_entry = entry + aux
                for _ in range(min(aux_count, _MAX_ENTRIES)):
                    _hash, _flags, _other, name, aux_next = _unpack("<IHHII", data, aux_entry)
                    names.add(_string(data, strings, name))
                    if aux_next == 0:
                        break
                    aux_entry += aux_next
                if next_entry == 0:
                    break
                entry += next_entry
    return info


def read(path: Path) -> ElfInfo:
    return parse(path.read_bytes())
