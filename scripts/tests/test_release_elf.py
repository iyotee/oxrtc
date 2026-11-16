"""release_elf.py: what a cross-built binary asks of the system, read without
running it.

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_elf  # noqa: E402
from elf_fixture import make_elf  # noqa: E402

GLIBC = {
    "libc.so.6": ["GLIBC_2.17", "GLIBC_2.34", "GLIBC_PRIVATE"],
    "libm.so.6": ["GLIBC_2.29"],
    "libgcc_s.so.1": ["GCC_3.0"],
}


class Parse(unittest.TestCase):
    def test_the_fields_a_release_check_needs_are_read(self):
        data = make_elf(183, "/lib/ld-linux-aarch64.so.1", ["libm.so.6", "libc.so.6"], GLIBC)
        info = release_elf.parse(data)
        self.assertEqual(info.machine, release_elf.EM_AARCH64)
        self.assertEqual(info.interpreter, "/lib/ld-linux-aarch64.so.1")
        self.assertEqual(info.needed, ["libm.so.6", "libc.so.6"])
        self.assertEqual(info.versions["libc.so.6"], {"GLIBC_2.17", "GLIBC_2.34", "GLIBC_PRIVATE"})
        self.assertEqual(info.versions["libgcc_s.so.1"], {"GCC_3.0"})

    def test_the_newest_glibc_version_ignores_private_and_other_libraries_tags(self):
        info = release_elf.parse(make_elf(62, "/lib64/ld-linux-x86-64.so.2", ["libc.so.6"], GLIBC))
        self.assertEqual(info.max_glibc(), (2, 34))

    def test_versions_compare_as_numbers_not_text(self):
        data = make_elf(62, None, ["libc.so.6"], {"libc.so.6": ["GLIBC_2.9", "GLIBC_2.10", "GLIBC_2.2.5"]})
        self.assertEqual(release_elf.parse(data).max_glibc(), (2, 10))

    def test_a_static_binary_needs_nothing(self):
        info = release_elf.parse(make_elf(62, None, [], {}))
        self.assertIsNone(info.interpreter)
        self.assertEqual(info.needed, [])
        self.assertIsNone(info.max_glibc())

    def test_glibc_version_names(self):
        self.assertEqual(release_elf.glibc_version("GLIBC_2.35"), (2, 35))
        self.assertEqual(release_elf.glibc_version("GLIBC_2.2.5"), (2, 2, 5))
        for other in ("GLIBC_PRIVATE", "GCC_3.0", "GLIBC_", "GLIBC_2", "xGLIBC_2.3"):
            self.assertIsNone(release_elf.glibc_version(other), other)


class Malformed(unittest.TestCase):
    def test_not_elf_and_not_elf64_le_are_refused(self):
        data = make_elf(62, None, [], {})
        for bad in (b"", b"MZ\x90\x00" + data[4:], data[:4] + b"\x01\x01" + data[6:], data[:4] + b"\x02\x02" + data[6:]):
            with self.assertRaises(release_elf.ElfError):
                release_elf.parse(bad)

    def test_every_truncation_is_an_elf_error_never_a_crash(self):
        data = make_elf(183, "/lib/ld-linux-aarch64.so.1", ["libc.so.6"], GLIBC)
        for length in range(len(data)):
            try:
                release_elf.parse(data[:length])
            except release_elf.ElfError:
                pass

    def test_corrupt_bytes_are_an_elf_error_never_a_crash(self):
        data = make_elf(183, "/lib/ld-linux-aarch64.so.1", ["libc.so.6"], GLIBC)
        for position in range(16, len(data)):
            corrupt = bytearray(data)
            corrupt[position] ^= 0xFF
            try:
                release_elf.parse(bytes(corrupt))
            except release_elf.ElfError:
                pass


if __name__ == "__main__":
    unittest.main()
