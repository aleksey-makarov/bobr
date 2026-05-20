#!/usr/bin/env python3
"""Verify object payload hashes in an mbuild store.

The tool walks direct children of <store>/objects, recomputes each payload hash
with fsobj-hash, and compares the computed hash with the object entry name.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


HEX64 = re.compile(r"^[0-9a-f]{64}$")


@dataclass(frozen=True)
class CheckResult:
    expected_hash: str
    object_path: Path
    actual_hash: str | None = None
    error: str | None = None

    @property
    def ok(self) -> bool:
        return self.error is None and self.actual_hash == self.expected_hash

    @property
    def mismatch(self) -> bool:
        return self.error is None and self.actual_hash != self.expected_hash


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Recompute fsobj hashes for every payload in <store>/objects "
            "and compare them with object entry names."
        )
    )
    parser.add_argument(
        "--store",
        type=Path,
        default=default_store(),
        help="store root (default: $MBUILD_STORE, ./mbuild-store, or ../mbuild-store)",
    )
    parser.add_argument(
        "--fsobj-hash",
        type=Path,
        default=None,
        help="path to fsobj-hash (default: PATH or target/debug/fsobj-hash)",
    )
    parser.add_argument(
        "-j",
        "--jobs",
        type=int,
        default=os.cpu_count() or 1,
        help="parallel hash jobs (default: CPU count)",
    )
    parser.add_argument(
        "--allow-non-object-entries",
        action="store_true",
        help="skip non-64-hex names under objects/ instead of failing",
    )
    args = parser.parse_args()

    try:
        store = args.store.resolve()
        fsobj_hash = resolve_fsobj_hash(args.fsobj_hash)
        objects = collect_objects(store, args.allow_non_object_entries)
        results = check_objects(objects, fsobj_hash, args.jobs)
        return print_results(results)
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


class CheckError(Exception):
    pass


def default_store() -> Path:
    if value := os.environ.get("MBUILD_STORE"):
        return Path(value)
    cwd_store = Path.cwd() / "mbuild-store"
    if cwd_store.is_dir():
        return cwd_store
    parent_store = Path.cwd().parent / "mbuild-store"
    if parent_store.is_dir():
        return parent_store
    return cwd_store


def resolve_fsobj_hash(explicit: Path | None) -> Path:
    if explicit is not None:
        if explicit.is_file() and os.access(explicit, os.X_OK):
            return explicit.resolve()
        raise CheckError(f"fsobj-hash is not executable: {explicit}")

    if path := shutil.which("fsobj-hash"):
        return Path(path)

    repo_binary = Path(__file__).resolve().parents[1] / "target" / "debug" / "fsobj-hash"
    if repo_binary.is_file() and os.access(repo_binary, os.X_OK):
        return repo_binary

    raise CheckError(
        "fsobj-hash not found; build it with `cargo build -p fsobj-hash` "
        "or pass --fsobj-hash"
    )


def collect_objects(store: Path, allow_non_object_entries: bool) -> list[Path]:
    objects_dir = store / "objects"
    if not objects_dir.is_dir():
        raise CheckError(f"store objects directory is missing: {objects_dir}")

    objects: list[Path] = []
    bad_names: list[str] = []
    for entry in sorted(objects_dir.iterdir(), key=lambda path: path.name):
        if not HEX64.fullmatch(entry.name):
            if allow_non_object_entries:
                continue
            bad_names.append(entry.name)
            continue
        objects.append(entry)

    if bad_names:
        joined = ", ".join(bad_names[:10])
        suffix = "" if len(bad_names) <= 10 else f", ... ({len(bad_names)} total)"
        raise CheckError(f"objects/ contains non-object entrie(s): {joined}{suffix}")

    return objects


def check_objects(objects: list[Path], fsobj_hash: Path, jobs: int) -> list[CheckResult]:
    if jobs < 1:
        raise CheckError("--jobs must be at least 1")

    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
        futures = [
            executor.submit(check_one_object, object_path, fsobj_hash)
            for object_path in objects
        ]
        return [future.result() for future in concurrent.futures.as_completed(futures)]


def check_one_object(object_path: Path, fsobj_hash: Path) -> CheckResult:
    expected_hash = object_path.name
    try:
        completed = subprocess.run(
            [str(fsobj_hash), str(object_path), "--mode=direct"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError as error:
        return CheckResult(expected_hash, object_path, error=str(error))

    if completed.returncode != 0:
        stderr = completed.stderr.strip()
        return CheckResult(
            expected_hash,
            object_path,
            error=stderr or f"fsobj-hash exited with status {completed.returncode}",
        )

    actual_hash = completed.stdout.strip()
    if not HEX64.fullmatch(actual_hash):
        return CheckResult(
            expected_hash,
            object_path,
            actual_hash=actual_hash,
            error=f"fsobj-hash printed invalid hash: {actual_hash!r}",
        )

    return CheckResult(expected_hash, object_path, actual_hash=actual_hash)


def print_results(results: list[CheckResult]) -> int:
    total = len(results)
    errors = sorted((result for result in results if result.error), key=sort_key)
    mismatches = sorted((result for result in results if result.mismatch), key=sort_key)

    for result in errors:
        print(f"ERROR {result.expected_hash} {result.object_path}: {result.error}", file=sys.stderr)
    for result in mismatches:
        print(
            f"MISMATCH {result.expected_hash} {result.object_path}: got {result.actual_hash}",
            file=sys.stderr,
        )

    ok_count = total - len(errors) - len(mismatches)
    print(
        f"checked {total} object(s): {ok_count} ok, "
        f"{len(mismatches)} mismatch(es), {len(errors)} error(s)"
    )

    if errors or mismatches:
        return 1
    return 0


def sort_key(result: CheckResult) -> str:
    return result.expected_hash


if __name__ == "__main__":
    sys.exit(main())
