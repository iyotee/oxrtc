"""release_package.py: reproducible archives with Unix modes from any build
machine, SHA256SUMS as sha256sum reads it, SBOMs without the checkout's path.

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import gzip
import hashlib
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import release_package as package  # noqa: E402

EPOCH = 1_763_251_200  # 2025-11-16T00:00:00Z


class Archives(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        (self.root / "oxrtc").write_bytes(b"\x7fELF binary")
        (self.root / "sbom.json").write_bytes(b"{}\n")
        self.entries = [
            package.Entry("README.md", data=b"readme\n"),
            package.Entry("oxrtc", self.root / "oxrtc", executable=True),
            package.Entry("sbom/oxrtc.cdx.json", self.root / "sbom.json"),
        ]

    def tearDown(self):
        self.directory.cleanup()

    def test_a_tar_carries_the_executable_bit_whatever_the_file_system_says(self):
        path = self.root / "a.tar.gz"
        package.write_tar_gz(path, "oxrtc-v1-linux-arm64", self.entries, EPOCH)
        with tarfile.open(path) as archive:
            members = {member.name: member for member in archive.getmembers()}
            self.assertEqual(
                sorted(members),
                ["oxrtc-v1-linux-arm64", "oxrtc-v1-linux-arm64/README.md", "oxrtc-v1-linux-arm64/oxrtc",
                 "oxrtc-v1-linux-arm64/sbom", "oxrtc-v1-linux-arm64/sbom/oxrtc.cdx.json"],
            )
            self.assertEqual(members["oxrtc-v1-linux-arm64/oxrtc"].mode, 0o755)
            self.assertEqual(members["oxrtc-v1-linux-arm64/README.md"].mode, 0o644)
            self.assertTrue(members["oxrtc-v1-linux-arm64/sbom"].isdir())
            for member in members.values():
                self.assertEqual((member.mtime, member.uid, member.gid), (EPOCH, 0, 0))
            self.assertEqual(archive.extractfile("oxrtc-v1-linux-arm64/oxrtc").read(), b"\x7fELF binary")

    def test_a_tar_is_the_same_bytes_twice_and_its_gzip_names_no_file(self):
        first, second = self.root / "1.tar.gz", self.root / "2.tar.gz"
        package.write_tar_gz(first, "top", self.entries, EPOCH)
        package.write_tar_gz(second, "top", list(reversed(self.entries)), EPOCH)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        header = first.read_bytes()[:10]
        self.assertEqual(int.from_bytes(header[4:8], "little"), EPOCH)
        self.assertEqual(header[3] & 0x08, 0, "no file name in the gzip header")
        self.assertTrue(gzip.decompress(first.read_bytes()))

    def test_a_zip_carries_unix_modes_and_is_the_same_bytes_twice(self):
        first, second = self.root / "1.zip", self.root / "2.zip"
        package.write_zip(first, "oxrtc-v1-windows-amd64", self.entries, EPOCH)
        package.write_zip(second, "oxrtc-v1-windows-amd64", list(reversed(self.entries)), EPOCH)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with zipfile.ZipFile(first) as archive:
            infos = {info.filename: info for info in archive.infolist()}
            self.assertEqual(infos["oxrtc-v1-windows-amd64/oxrtc"].external_attr >> 16, 0o100755)
            self.assertEqual(infos["oxrtc-v1-windows-amd64/README.md"].external_attr >> 16, 0o100644)
            self.assertEqual(infos["oxrtc-v1-windows-amd64/README.md"].date_time, (2025, 11, 16, 0, 0, 0))
            self.assertEqual(archive.read("oxrtc-v1-windows-amd64/sbom/oxrtc.cdx.json"), b"{}\n")

    def test_a_name_used_twice_is_refused(self):
        twice = self.entries + [package.Entry("README.md", self.root / "sbom.json")]
        with self.assertRaises(ValueError):
            package.Entry("empty").content()
        with self.assertRaises(ValueError):
            package.write_zip(self.root / "x.zip", "top", twice, EPOCH)
        with self.assertRaises(ValueError):
            package.write_tar_gz(self.root / "x.tar.gz", "top", twice, EPOCH)


class Sums(unittest.TestCase):
    def test_sha256sum_format_sorted_and_checked(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "b.tar.gz").write_bytes(b"b")
            (root / "a.zip").write_bytes(b"a")
            (root / "unlisted").write_bytes(b"not part of this release")
            package.write_sums(root, ["b.tar.gz", "a.zip"])
            text = (root / package.SUMS).read_bytes()
            self.assertEqual(
                text,
                f"{hashlib.sha256(b'a').hexdigest()}  a.zip\n{hashlib.sha256(b'b').hexdigest()}  b.tar.gz\n".encode(),
            )
            self.assertEqual(package.verify_sums(root), ["a.zip", "b.tar.gz"])
            (root / "a.zip").write_bytes(b"changed")
            with self.assertRaises(ValueError):
                package.verify_sums(root)

    def test_a_later_run_into_the_directory_keeps_the_assets_already_listed(self):
        # The README's two commands share one directory: all three, then one platform again.
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            for asset in ("linux-arm64.tar.gz", "windows-amd64.zip", "gone.tar.gz"):
                (root / asset).write_bytes(asset.encode())
            package.write_sums(root, ["linux-arm64.tar.gz", "windows-amd64.zip", "gone.tar.gz"])
            (root / "gone.tar.gz").unlink()
            (root / "linux-arm64.tar.gz").write_bytes(b"rebuilt")
            package.write_sums(root, ["linux-arm64.tar.gz"])
            self.assertEqual(package.verify_sums(root), ["linux-arm64.tar.gz", "windows-amd64.zip"])
            self.assertIn(f"{hashlib.sha256(b'rebuilt').hexdigest()}  linux-arm64.tar.gz\n",
                          (root / package.SUMS).read_text(encoding="ascii"))
            # A kept asset changed since it was listed is not listed anew: the check fails.
            (root / "windows-amd64.zip").write_bytes(b"tampered")
            package.write_sums(root, ["linux-arm64.tar.gz"])
            with self.assertRaisesRegex(ValueError, "windows-amd64.zip: checksum mismatch"):
                package.verify_sums(root)

    def test_the_sums_never_list_themselves(self):
        with tempfile.TemporaryDirectory() as name, self.assertRaises(ValueError):
            package.write_sums(Path(name), [package.SUMS])


class Sbom(unittest.TestCase):
    def test_the_checkout_path_is_replaced_on_windows_and_linux(self):
        cases = [
            (Path("K:/src/oxrtc"), "path+file:///K:/src/oxrtc/apps/oxrtc#0.1.0"),
            (Path("K:/src/oxrtc"), "path+file:///k:/src/oxrtc/apps/oxrtc#0.1.0"),
            (Path("K:/src/oxrtc"), "path+file:///K%3A/src/oxrtc/apps/oxrtc#0.1.0"),
        ]
        if sys.platform != "win32":
            cases = [(Path("/home/runner/work/oxrtc"), "path+file:///home/runner/work/oxrtc/apps/oxrtc#0.1.0")]
        for root, reference in cases:
            normalized = package.normalize_sbom(f'{{"bom-ref": "{reference}"}}', root)
            self.assertEqual(normalized, '{"bom-ref": "path+file:///oxrtc/apps/oxrtc#0.1.0"}', reference)

    def test_a_sibling_directory_with_the_same_prefix_is_left(self):
        root = Path("K:/src/oxrtc") if sys.platform == "win32" else Path("/src/oxrtc")
        prefix = root.resolve().as_posix().strip("/")
        text = f'"path+file:///{prefix}-other/apps/x#1"'
        self.assertEqual(package.normalize_sbom(text, root), text)


if __name__ == "__main__":
    unittest.main()
