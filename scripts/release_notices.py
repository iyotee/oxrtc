"""THIRD_PARTY_NOTICES.txt for a release archive (R16), from cargo-about's JSON.

cargo-about (scripts/about.toml) resolves each crate's licence expression to
the licences it is used under and finds their texts in the crate's own files.
This module writes them as one plain-text file: the crates, the licence texts
grouped as cargo-about grouped them, and the files themselves for the crates
whose terms one licence text does not carry - a compound expression (`ISC AND
(Apache-2.0 OR ISC) AND ...`, aws-lc's) or a notice file of their own. The
workspace's members are oxrtc and are left out; a crate patched in from
`third_party/` (`[patch.crates-io]`) has no registry source either, but it is
not a member: it is third-party and listed like the others.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

# Files whose name starts with one of these are licence or notice files.
_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")
# A notice file of its own makes a crate's files carried verbatim.
_NOTICE_PREFIXES = ("NOTICE", "LICENSE-THIRD-PARTY", "THIRD-PARTY", "THIRDPARTY")
_SKIPPED_DIRS = {".git", "target", "tests", "test", "benches", "examples", "fuzz"}
_MAX_DEPTH = 5
_MAX_FILE_BYTES = 1 << 20
_RULE = "=" * 78


def _third_party(package: dict, own: set[str]) -> bool:
    """Not one of the workspace's members: a registry crate, or a path one
    that is not ours (a patched copy such as third_party/h264-reader)."""
    return package.get("source") is not None or package["name"] not in own


def _crate_key(package: dict) -> tuple[str, str]:
    return (package["name"], package["version"])


def _needs_files(package: dict, root: Path) -> bool:
    expression = package.get("license") or ""
    if " AND " in expression:
        return True
    try:
        return any(entry.name.upper().startswith(_NOTICE_PREFIXES) for entry in root.iterdir() if entry.is_file())
    except OSError:
        return False


def licence_files(root: Path) -> list[Path]:
    """Every licence or notice file beneath a crate's root, tests and examples
    excluded, sorted, at most _MAX_DEPTH directories down."""
    found: list[Path] = []

    def walk(directory: Path, depth: int) -> None:
        try:
            entries = sorted(directory.iterdir(), key=lambda entry: entry.name)
        except OSError:
            return
        for entry in entries:
            if entry.is_dir():
                if depth < _MAX_DEPTH and entry.name.lower() not in _SKIPPED_DIRS:
                    walk(entry, depth + 1)
            elif entry.is_file() and entry.name.upper().startswith(_PREFIXES):
                found.append(entry)

    walk(root, 0)
    return found


def render(reports: list[dict], title: str, own: set[str]) -> str:
    """One notices file from one or more cargo-about JSON reports (one per
    binary in the archive: their crates are merged, each listed once). `own`
    names the workspace's members, the only crates left out."""
    if not own:
        raise ValueError("no workspace member named: every path crate would be taken for oxrtc's")
    crates: dict[tuple[str, str], dict] = {}
    groups: dict[str, dict] = {}
    for report in reports:
        for crate in report.get("crates", []):
            package = crate["package"]
            if _third_party(package, own):
                crates.setdefault(_crate_key(package), package)
        for licence in report.get("licenses", []):
            users = sorted({_crate_key(user["crate"]) for user in licence.get("used_by", []) if _third_party(user["crate"], own)})
            if not users:
                continue
            group = groups.setdefault(licence["text"], {"id": licence["id"], "name": licence["name"], "users": set()})
            group["users"].update(users)

    lines = [
        title,
        "",
        "oxrtc itself is licensed MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE).",
        f"It is built from the {len(crates)} third-party crates below, each used",
        "under the licence named; their licence texts follow, then the licence and",
        "notice files of the crates whose terms one text does not carry.",
        "",
        _RULE,
        "Crates",
        _RULE,
        "",
    ]
    for name, version in sorted(crates):
        package = crates[(name, version)]
        repository = package.get("repository") or ""
        lines.append(f"{name} {version}  {package.get('license') or 'see its files'}  {repository}".rstrip())

    lines += ["", _RULE, "Licence texts", _RULE]
    for text, group in sorted(groups.items(), key=lambda item: (item[1]["id"], sorted(item[1]["users"]))):
        users = ", ".join(f"{name} {version}" for name, version in sorted(group["users"]))
        lines += ["", f"--- {group['name']} ({group['id']}), used by: {users}", "", text.rstrip(), ""]

    seen: set[str] = set()
    verbatim: list[str] = []
    for key in sorted(crates):
        package = crates[key]
        root = Path(package["manifest_path"]).parent
        if not _needs_files(package, root):
            continue
        for path in licence_files(root):
            try:
                if path.stat().st_size > _MAX_FILE_BYTES:
                    continue
                content = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            digest = hashlib.sha256(content.encode("utf-8")).hexdigest()
            relative = path.relative_to(root).as_posix()
            if digest in seen:
                verbatim += ["", f"--- {key[0]} {key[1]}: {relative} (the same text as above)"]
                continue
            seen.add(digest)
            verbatim += ["", f"--- {key[0]} {key[1]}: {relative}", "", content.rstrip(), ""]
    if verbatim:
        lines += ["", _RULE, "Licence and notice files as the crates ship them", _RULE] + verbatim
    return "\n".join(lines).rstrip() + "\n"


def render_files(paths: list[Path], title: str, own: set[str]) -> str:
    return render([json.loads(path.read_text(encoding="utf-8")) for path in paths], title, own)
