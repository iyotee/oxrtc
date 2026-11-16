"""release_pe.py: what the Windows binary asks of the system, read from its
PE headers.

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_pe  # noqa: E402
from pe_fixture import make_pe  # noqa: E402

# What oxrtc.exe of 71a7a61 imported: built without +crt-static.
DYNAMIC_CRT = ["kernel32.dll", "ws2_32.dll", "VCRUNTIME140.dll", "api-ms-win-crt-runtime-l1-1-0.dll",
               "api-ms-win-crt-heap-l1-1-0.dll", "api-ms-win-core-synch-l1-2-0.dll"]


class Parse(unittest.TestCase):
    def test_the_machine_and_the_imports_direct_and_delayed_are_read(self):
        info = release_pe.parse(make_pe(0x8664, ["kernel32.dll", "bcrypt.dll"], ["iphlpapi.dll"]))
        self.assertEqual(info.machine, release_pe.IMAGE_FILE_MACHINE_AMD64)
        self.assertEqual(info.imports, ["kernel32.dll", "bcrypt.dll", "iphlpapi.dll"])

    def test_the_c_runtime_is_named_and_the_systems_own_dlls_are_not(self):
        info = release_pe.parse(make_pe(0x8664, DYNAMIC_CRT, ["MSVCP140.dll", "ucrtbase.dll"]))
        self.assertEqual(info.c_runtime(), ["MSVCP140.dll", "VCRUNTIME140.dll", "api-ms-win-crt-heap-l1-1-0.dll",
                                            "api-ms-win-crt-runtime-l1-1-0.dll", "ucrtbase.dll"])

    def test_a_static_c_runtime_imports_none_of_it(self):
        info = release_pe.parse(make_pe(0x8664, ["kernel32.dll", "ntdll.dll", "api-ms-win-core-synch-l1-2-0.dll"]))
        self.assertEqual(info.c_runtime(), [])

    def test_no_import_at_all_is_read(self):
        self.assertEqual(release_pe.parse(make_pe(0x8664, [])).imports, [])


class Malformed(unittest.TestCase):
    def test_not_mz_not_pe_and_not_pe32_plus_are_refused(self):
        data = make_pe(0x8664, ["kernel32.dll"])
        no_signature = bytearray(data)
        no_signature[0x40:0x44] = b"XX\0\0"
        pe32 = bytearray(data)
        pe32[0x40 + 24] = 0x0B
        pe32[0x40 + 25] = 0x01
        for bad in (b"", b"\x7fELF" + data[4:], bytes(no_signature), bytes(pe32)):
            with self.assertRaises(release_pe.PeError):
                release_pe.parse(bad)

    def test_every_truncation_is_a_pe_error_never_a_crash(self):
        data = make_pe(0x8664, ["kernel32.dll", "VCRUNTIME140.dll"], ["iphlpapi.dll"])
        for length in range(len(data)):
            try:
                release_pe.parse(data[:length])
            except release_pe.PeError:
                pass

    def test_corrupt_bytes_are_a_pe_error_never_a_crash(self):
        data = make_pe(0x8664, ["kernel32.dll", "VCRUNTIME140.dll"], ["iphlpapi.dll"])
        for position in range(2, len(data)):
            corrupt = bytearray(data)
            corrupt[position] ^= 0xFF
            try:
                release_pe.parse(bytes(corrupt))
            except release_pe.PeError:
                pass


if __name__ == "__main__":
    unittest.main()
