#!/usr/bin/env python3
"""Drop an mbuild store object by publication name.

The argument is a name from object-refs/.  The tool removes every store index
entry that points to the same object identity, not just the requested public
name, so that build/reuse refs cannot resurrect the dropped object.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import stat
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


HEX64 = re.compile(r"^[0-9a-f]{64}$")
STORE_NAME = re.compile(r"^[A-Za-z0-9._-]+$")


@dataclass(frozen=True)
class DropPlan:
    store: Path
    name: str
    object_hash: str
    object_path: Path
    result_ids: tuple[str, ...]
    object_refs: tuple[Path, ...]
    result_refs: tuple[Path, ...]
    build_refs: tuple[Path, ...]
    reuse_refs: tuple[Path, ...]
    result_paths: tuple[Path, ...]


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Drop an mbuild store object by a name from object-refs/. "
            "Dry-run by default; pass --yes to delete."
        )
    )
    parser.add_argument(
        "name",
        help=(
            "publication name from <store>/object-refs; a 64-char object hash "
            "is also accepted for resuming a partially completed deletion"
        ),
    )
    parser.add_argument(
        "--store",
        type=Path,
        default=default_store(),
        help="store root (default: $MBUILD_STORE, ./mbuild-store, or ../mbuild-store)",
    )
    parser.add_argument(
        "--yes",
        action="store_true",
        help="delete the planned refs, result records, and object payload",
    )
    args = parser.parse_args()

    try:
        plan = build_drop_plan(args.store, args.name)
        print_plan(plan, deleting=args.yes)
        if args.yes:
            quarantine_path = execute_drop_plan(plan)
            if quarantine_path is not None:
                print(f"object payload moved to quarantine: {quarantine_path}")
            print("deleted")
        else:
            print("dry-run only; pass --yes to delete")
    except StoreDropError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


class StoreDropError(Exception):
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


def build_drop_plan(store: Path, name: str) -> DropPlan:
    store = store.resolve()
    validate_name(name)
    require_store_layout(store)

    object_ref = store / "object-refs" / name
    if object_ref.is_symlink():
        object_hash = parse_object_ref_target(object_ref, os.readlink(object_ref))
    elif HEX64.fullmatch(name) and (store / "objects" / name).exists():
        object_hash = name
    else:
        raise StoreDropError(f"object ref '{object_ref}' is missing or is not a symlink")
    object_path = store / "objects" / object_hash

    result_ids = find_result_ids_for_object(store, object_hash)
    result_id_set = set(result_ids)

    object_refs = find_refs_to_target(
        store / "object-refs",
        expected_dir="objects",
        expected_names={object_hash},
        suffix="",
    )
    result_refs = find_refs_to_target(
        store / "result-refs",
        expected_dir="results",
        expected_names={f"{result_id}.json" for result_id in result_id_set},
        suffix=".json",
    )
    build_refs = find_refs_to_target(
        store / "builds",
        expected_dir="results",
        expected_names={f"{result_id}.json" for result_id in result_id_set},
        suffix="",
    )
    reuse_refs = find_refs_to_target(
        store / "reuses",
        expected_dir="results",
        expected_names={f"{result_id}.json" for result_id in result_id_set},
        suffix="",
    )
    result_paths = tuple(
        sorted((store / "results" / f"{result_id}.json" for result_id in result_ids))
    )

    return DropPlan(
        store=store,
        name=name,
        object_hash=object_hash,
        object_path=object_path,
        result_ids=tuple(result_ids),
        object_refs=object_refs,
        result_refs=result_refs,
        build_refs=build_refs,
        reuse_refs=reuse_refs,
        result_paths=result_paths,
    )


def validate_name(name: str) -> None:
    if not name:
        raise StoreDropError("object ref name must not be empty")
    if "/" in name or "\\" in name:
        raise StoreDropError("object ref name must be a single path component")
    if not STORE_NAME.fullmatch(name):
        raise StoreDropError(
            f"invalid object ref name '{name}'; allowed chars: [A-Za-z0-9._-]"
        )


def require_store_layout(store: Path) -> None:
    required = ["object-refs", "result-refs", "objects", "results", "builds", "reuses"]
    missing = [name for name in required if not (store / name).is_dir()]
    if missing:
        joined = ", ".join(missing)
        raise StoreDropError(f"store '{store}' is missing required dir(s): {joined}")


def parse_object_ref_target(ref: Path, target: str) -> str:
    parts = Path(target).parts
    if len(parts) != 3 or parts[0] != ".." or parts[1] != "objects":
        raise StoreDropError(
            f"object ref '{ref}' points outside ../objects/: target is '{target}'"
        )
    object_hash = parts[2]
    if not HEX64.fullmatch(object_hash):
        raise StoreDropError(
            f"object ref '{ref}' points to invalid object hash '{object_hash}'"
        )
    return object_hash


def find_result_ids_for_object(store: Path, object_hash: str) -> list[str]:
    out: list[str] = []
    for path in sorted((store / "results").glob("*.json")):
        if not path.is_file():
            continue
        result_id = path.name.removesuffix(".json")
        if not HEX64.fullmatch(result_id):
            raise StoreDropError(f"invalid result record file name '{path.name}'")
        try:
            with path.open("rb") as handle:
                record = json.load(handle)
        except json.JSONDecodeError as error:
            raise StoreDropError(f"failed to parse result record '{path}': {error}") from error
        if not isinstance(record, dict):
            raise StoreDropError(f"result record '{path}' is not a JSON object")
        if record.get("object_hash") == object_hash:
            encoded_result_id = record.get("result_id")
            if encoded_result_id is not None and encoded_result_id != result_id:
                raise StoreDropError(
                    f"result record '{path}' encodes result_id '{encoded_result_id}'"
                )
            out.append(result_id)
    return out


def find_refs_to_target(
    directory: Path,
    *,
    expected_dir: str,
    expected_names: set[str],
    suffix: str,
) -> tuple[Path, ...]:
    refs: list[Path] = []
    if not expected_names:
        return tuple()
    for path in sorted(directory.iterdir()):
        if suffix and not path.name.endswith(suffix):
            continue
        if not path.is_symlink():
            continue
        target = os.readlink(path)
        parts = Path(target).parts
        if len(parts) == 3 and parts[0] == ".." and parts[1] == expected_dir:
            if parts[2] in expected_names:
                refs.append(path)
    return tuple(refs)


def print_plan(plan: DropPlan, *, deleting: bool) -> None:
    mode = "DELETE" if deleting else "DRY-RUN"
    print(f"{mode}: drop object publication '{plan.name}'")
    print(f"store: {plan.store}")
    print(f"object_hash: {plan.object_hash}")
    print(f"object_path: {plan.object_path}")
    print_paths("object refs", plan.object_refs)
    print_values("result ids", plan.result_ids)
    print_paths("result refs", plan.result_refs)
    print_paths("build refs", plan.build_refs)
    print_paths("reuse refs", plan.reuse_refs)
    print_paths("result records", plan.result_paths)


def print_values(label: str, values: Iterable[str]) -> None:
    values = tuple(values)
    print(f"{label}: {len(values)}")
    for value in values:
        print(f"  {value}")


def print_paths(label: str, paths: Iterable[Path]) -> None:
    paths = tuple(paths)
    print(f"{label}: {len(paths)}")
    for path in paths:
        print(f"  {path}")


def execute_drop_plan(plan: DropPlan) -> Path | None:
    quarantine_path = None
    if plan.object_path.is_symlink():
        raise StoreDropError(f"object path '{plan.object_path}' is unexpectedly a symlink")
    if plan.object_path.is_dir():
        try:
            remove_dir_force(plan.object_path)
        except OSError as error:
            quarantine_path = quarantine_object_path(plan, error)
    elif plan.object_path.exists():
        plan.object_path.unlink()

    for path in (
        *plan.object_refs,
        *plan.result_refs,
        *plan.build_refs,
        *plan.reuse_refs,
        *plan.result_paths,
    ):
        remove_file_or_symlink(path)
    return quarantine_path


def remove_file_or_symlink(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.exists():
        raise StoreDropError(f"ref/record path '{path}' is not a file or symlink")


def remove_dir_force(path: Path) -> None:
    make_tree_dirs_writable(path)
    shutil.rmtree(path)


def make_tree_dirs_writable(path: Path) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        return

    desired = metadata.st_mode | 0o700
    if desired != metadata.st_mode:
        path.chmod(stat.S_IMODE(desired))

    with os.scandir(path) as entries:
        for entry in entries:
            make_tree_dirs_writable(Path(entry.path))


def quarantine_object_path(plan: DropPlan, error: OSError) -> Path:
    quarantine_dir = plan.store / "quarantine"
    quarantine_dir.mkdir(parents=True, exist_ok=True)
    timestamp = time.strftime("%y%m%d%H%M%S", time.localtime())
    base = f"{timestamp}-drop-object-{plan.object_hash}"
    for index in range(1, 1000):
        suffix = "" if index == 1 else f"-{index}"
        target = quarantine_dir / f"{base}{suffix}"
        try:
            plan.object_path.rename(target)
        except FileExistsError:
            continue
        except OSError as rename_error:
            raise StoreDropError(
                "failed to remove object payload "
                f"'{plan.object_path}': {error}; "
                f"also failed to quarantine it at '{target}': {rename_error}"
            ) from rename_error
        write_quarantine_metadata(target, plan, str(error))
        return target
    raise StoreDropError(
        f"failed to allocate quarantine path for object '{plan.object_hash}'"
    )


def write_quarantine_metadata(target: Path, plan: DropPlan, reason: str) -> None:
    metadata = {
        "schema": "mbuild-store-drop-quarantine-v1",
        "object_hash": plan.object_hash,
        "original_object_path": str(plan.object_path),
        "quarantine_path": str(target),
        "reason": reason,
    }
    metadata_path = target.with_name(f"{target.name}.json")
    metadata_path.write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
