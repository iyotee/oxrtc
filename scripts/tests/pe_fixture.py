"""A minimal PE32+ image with the parts release_pe.py reads: the machine, an
import directory and a delay-import directory in one section."""

from __future__ import annotations

import struct

_RAW = 0x200
_VIRTUAL = 0x1000


def make_pe(machine: int, imports: list[str], delayed: list[str] | None = None) -> bytes:
    delayed = delayed or []
    imports_at = 0
    delayed_at = imports_at + 20 * (len(imports) + 1)
    names_at = delayed_at + 32 * (len(delayed) + 1)
    names = bytearray()
    rvas = []
    for dll in imports + delayed:
        rvas.append(_VIRTUAL + names_at + len(names))
        names += dll.encode("ascii") + b"\0"
    body = bytearray()
    for rva in rvas[: len(imports)]:
        body += struct.pack("<IIIII", 0, 0, 0, rva, 0)
    body += bytes(20)
    for rva in rvas[len(imports) :]:
        body += struct.pack("<IIIIIIII", 1, rva, 0, 0, 0, 0, 0, 0)
    body += bytes(32) + names

    directories = [(0, 0)] * 16
    if imports:
        directories[1] = (_VIRTUAL + imports_at, 20 * (len(imports) + 1))
    if delayed:
        directories[13] = (_VIRTUAL + delayed_at, 32 * (len(delayed) + 1))
    optional = bytearray(112)
    struct.pack_into("<H", optional, 0, 0x20B)
    struct.pack_into("<I", optional, 108, len(directories))
    optional += b"".join(struct.pack("<II", *entry) for entry in directories)

    pe = 0x40
    dos = bytearray(pe)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, pe)
    coff = struct.pack("<HHIIIHH", machine, 1, 0, 0, 0, len(optional), 0x22)
    section = struct.pack("<8sIIIIIIHHI", b".idata", len(body), _VIRTUAL, len(body), _RAW, 0, 0, 0, 0, 0x40000040)
    headers = bytes(dos) + b"PE\0\0" + coff + bytes(optional) + section
    return headers.ljust(_RAW, b"\0") + bytes(body)
