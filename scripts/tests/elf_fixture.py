"""A minimal ELF64 little-endian file with the parts release_elf.py reads:
a program interpreter, loadable segments, DT_NEEDED entries and a GNU
version-needs section."""

from __future__ import annotations

import struct

SHT_STRTAB = 3
SHT_DYNAMIC = 6
SHT_GNU_VERNEED = 0x6FFFFFFE
# What cargo-zigbuild's and GNU ld's AArch64 binaries carry: 64 KiB.
ALIGNED = [(0, 0, 0x10000), (0x23C240, 0x24C240, 0x10000)]


def make_elf(
    machine: int,
    interpreter: str | None,
    needed: list[str],
    versions: dict[str, list[str]],
    loads: list[tuple[int, int, int]] | None = None,
) -> bytes:
    loads = ALIGNED if loads is None else loads
    strings = bytearray(b"\0")
    offsets: dict[str, int] = {}

    def string(text: str) -> int:
        if text not in offsets:
            offsets[text] = len(strings)
            strings.extend(text.encode() + b"\0")
        return offsets[text]

    dynamic = b"".join(struct.pack("<qQ", 1, string(name)) for name in needed) + struct.pack("<qQ", 0, 0)
    verneed = bytearray()
    libraries = list(versions.items())
    for index, (library, names) in enumerate(libraries):
        last = index == len(libraries) - 1
        entry_size = 16 + 16 * len(names)
        verneed += struct.pack("<HHIII", 1, len(names), string(library), 16, 0 if last else entry_size)
        for position, name in enumerate(names):
            following = 0 if position == len(names) - 1 else 16
            verneed += struct.pack("<IHHII", 0, 0, 0, string(name), following)

    header_size, phdr_size, shdr_size = 64, 56, 64
    phnum = (1 if interpreter is not None else 0) + len(loads)
    cursor = header_size + phnum * phdr_size
    interp = (interpreter.encode() + b"\0") if interpreter is not None else b""
    interp_offset = cursor
    cursor += len(interp)
    strings_offset = cursor
    cursor += len(strings)
    dynamic_offset = cursor
    cursor += len(dynamic)
    verneed_offset = cursor
    cursor += len(verneed)
    shoff = cursor

    sections = [
        struct.pack("<IIQQQQIIQQ", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
        struct.pack("<IIQQQQIIQQ", 0, SHT_STRTAB, 0, 0, strings_offset, len(strings), 0, 0, 1, 0),
        struct.pack("<IIQQQQIIQQ", 0, SHT_DYNAMIC, 0, 0, dynamic_offset, len(dynamic), 1, 0, 8, 16),
    ]
    if libraries:
        sections.append(
            struct.pack("<IIQQQQIIQQ", 0, SHT_GNU_VERNEED, 0, 0, verneed_offset, len(verneed), 1, len(libraries), 4, 0)
        )
    ident = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header = struct.pack(
        "<16sHHIQQQIHHHHHH", ident, 3, machine, 1, 0, header_size, shoff, 0,
        header_size, phdr_size, phnum, shdr_size, len(sections), 0,
    )
    program = struct.pack("<IIQQQQQQ", 3, 4, interp_offset, 0, 0, len(interp), len(interp), 1) if interp else b""
    program += b"".join(struct.pack("<IIQQQQQQ", 1, 5, offset, address, address, 0, 0, align) for offset, address, align in loads)
    return header + program + interp + bytes(strings) + dynamic + bytes(verneed) + b"".join(sections)
