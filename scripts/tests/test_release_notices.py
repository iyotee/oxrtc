"""release_notices.py: the third-party notices of an archive (R16).

Run: python -m unittest discover -s scripts/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import release_notices as notices  # noqa: E402

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


class Render(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def crate(self, name: str, licence: str, files: dict[str, str], source: str | None = REGISTRY) -> dict:
        root = self.root / f"{name}-1.0.0"
        for relative, text in files.items():
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / relative).write_bytes(text.encode())
        (root / "Cargo.toml").parent.mkdir(parents=True, exist_ok=True)
        (root / "Cargo.toml").write_bytes(b"[package]\n")
        return {"name": name, "version": "1.0.0", "source": source, "license": licence,
                "repository": f"https://example.invalid/{name}", "manifest_path": str(root / "Cargo.toml")}

    @staticmethod
    def report(crates: list[dict], licences: list[tuple[str, str, str, list[dict]]]) -> dict:
        return {
            "crates": [{"package": crate, "license": crate["license"]} for crate in crates],
            "licenses": [{"id": id_, "name": name, "text": text, "used_by": [{"crate": user} for user in users]}
                         for id_, name, text, users in licences],
        }

    def test_the_workspace_crates_are_not_third_party(self):
        ours = self.crate("oxrtc-core", "MIT OR Apache-2.0", {}, source=None)
        theirs = self.crate("bytes", "MIT", {"LICENSE": "MIT text"})
        text = notices.render([self.report([ours, theirs], [
            ("MIT", "MIT License", "MIT text of bytes", [ours, theirs]),
            ("Apache-2.0", "Apache License 2.0", "only ours", [ours]),
        ])], "title")
        self.assertIn("bytes 1.0.0  MIT  https://example.invalid/bytes", text)
        self.assertIn("used by: bytes 1.0.0\n", text)
        self.assertNotIn("oxrtc-core", text)
        self.assertNotIn("only ours", text)
        self.assertIn("the 1 third-party crates", text)

    def test_reports_of_two_binaries_list_each_crate_once_and_merge_users(self):
        a = self.crate("rustix", "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT", {})
        b = self.crate("tokio", "MIT", {})
        text = notices.render([
            self.report([a], [("MIT", "MIT License", "the MIT text", [a])]),
            self.report([a, b], [("MIT", "MIT License", "the MIT text", [a, b])]),
        ], "title")
        self.assertEqual(text.count("\nrustix 1.0.0 "), 1)
        self.assertEqual(text.count("the MIT text"), 1)
        self.assertIn("used by: rustix 1.0.0, tokio 1.0.0\n", text)

    def test_a_compound_licence_carries_its_files_nested_ones_included_tests_excluded(self):
        aws = self.crate("aws-lc-sys", "ISC AND (Apache-2.0 OR ISC) AND OpenSSL", {
            "LICENSE": "AWS-LC top licence with the OpenSSL and SSLeay notices",
            "aws-lc/LICENSE": "the C library's own copy, slightly different",
            "aws-lc/third_party/fiat/LICENSE": "fiat-crypto MIT",
            "aws-lc/tests/LICENSE": "a test fixture, not shipped code",
            "aws-lc/crypto/ssl.c": "int main;",
        })
        plain = self.crate("itoa", "MIT OR Apache-2.0", {"LICENSE-MIT": "itoa's MIT"})
        text = notices.render([self.report([aws, plain], [])], "title")
        self.assertIn("--- aws-lc-sys 1.0.0: LICENSE\n\nAWS-LC top licence with the OpenSSL and SSLeay notices", text)
        self.assertIn("--- aws-lc-sys 1.0.0: aws-lc/LICENSE\n", text)
        self.assertIn("--- aws-lc-sys 1.0.0: aws-lc/third_party/fiat/LICENSE\n\nfiat-crypto MIT", text)
        self.assertNotIn("a test fixture", text)
        self.assertNotIn("int main", text)
        self.assertNotIn("itoa's MIT", text, "a crate whose terms one text carries is not copied whole")

    def test_a_notice_file_carries_the_crates_files_and_one_text_is_written_once(self):
        a = self.crate("atomic-waker", "Apache-2.0 OR MIT", {"LICENSE-THIRD-PARTY": "futures' notice", "LICENSE-MIT": "shared"})
        b = self.crate("other", "MIT AND BSD-3-Clause", {"LICENSE": "shared"})
        text = notices.render([self.report([a, b], [])], "title")
        self.assertIn("--- atomic-waker 1.0.0: LICENSE-THIRD-PARTY\n\nfutures' notice", text)
        self.assertEqual(text.count("\nshared\n"), 1)
        self.assertIn("--- other 1.0.0: LICENSE (the same text as above)", text)

    def test_nothing_to_carry_writes_no_empty_section(self):
        crate = self.crate("tokio", "MIT", {})
        text = notices.render([self.report([crate], [])], "title")
        self.assertNotIn("as the crates ship them", text)
        self.assertTrue(text.startswith("title\n") and text.endswith("\n") and not text.endswith("\n\n"))


if __name__ == "__main__":
    unittest.main()
