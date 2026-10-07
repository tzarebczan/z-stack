#!/usr/bin/env python3
"""Resume a pinned archive on disk, verify it, then restore owned staging.

The archive survives interrupted downloads/extraction and is removed only after
verified state is promoted. Existing staging requires a matching source marker.
"""
import importlib.util
from compression.zstd import ZstdFile
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile
import tempfile
import time

spec = importlib.util.spec_from_file_location("snapshot", Path(__file__).with_name("node-snapshot.py"))
snapshot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(snapshot)
CHUNK = 4 * 1024 * 1024


def release_cache(file, writing=False):
    if writing:
        file.flush()
        os.fdatasync(file.fileno())
    if hasattr(os, "posix_fadvise"):
        os.posix_fadvise(file.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)


def source_matches(path, metadata):
    if not path.is_file() or path.is_symlink():
        return False
    source = json.loads(path.read_text())
    return all(source.get(k) == metadata[k] for k in ("sha256", "size_bytes", "url"))


def cached_archive(metadata, cache, progress, reserve):
    partial = cache.with_name(cache.name + ".partial")
    source = cache.with_name(cache.name + ".source.json")
    if cache.is_symlink() or partial.is_symlink():
        raise ValueError("archive paths must not be symbolic links")
    if cache.exists() or partial.exists():
        if not source_matches(source, metadata):
            raise ValueError("existing archive has no matching pinned source")
    else:
        snapshot.write_json(source, metadata)
    existing = cache if cache.exists() else partial
    reader = snapshot.Download(metadata, progress, cache.parent, reserve)
    reader.phase = "downloading-cache"
    try:
        if existing.exists():
            if existing.stat().st_size > metadata["size_bytes"]:
                raise ValueError("cached archive is larger than the pinned size")
            snapshot.write_json(progress, {"phase": "verifying-cache-prefix", "cached_bytes": existing.stat().st_size})
            with existing.open("rb", buffering=0) as file:
                while chunk := file.read(CHUNK):
                    reader.offset += len(chunk)
                    reader.digest.update(chunk)
                    release_cache(file)
        reader.initial_offset = reader.offset
        reader.started = time.monotonic()
        remaining = metadata["size_bytes"] - reader.offset
        if cache.exists() and remaining:
            raise ValueError("completed archive is shorter than the pinned size")
        if shutil.disk_usage(cache.parent).free < reserve + remaining:
            raise OSError("archive download would consume reserved free disk space")
        if not cache.exists():
            with partial.open("ab") as file:
                pending = 0
                while chunk := reader.read(CHUNK):
                    file.write(chunk)
                    pending += len(chunk)
                    if pending >= 16 * 1024 * 1024:
                        release_cache(file, writing=True)
                        pending = 0
                release_cache(file, writing=True)
        if reader.offset != metadata["size_bytes"] or reader.digest.hexdigest() != metadata["sha256"]:
            raise ValueError("cached snapshot SHA-256 or size mismatch; preserved for diagnosis")
        if not cache.exists():
            partial.replace(cache)
        snapshot.write_json(progress, {"phase": "archive-verified", "sha256": metadata["sha256"]})
        return cache
    finally:
        reader.close()


def restore(metadata, target, cache, progress, reserve):
    marker = target / "snapshot-verified.json"
    if target.exists():
        if source_matches(marker, metadata):
            return
        raise FileExistsError("refusing to replace an existing node directory")
    staging = target.with_name(target.name + ".partial")
    source = staging / ".snapshot-source.json"
    if staging.is_symlink():
        raise ValueError("staging must not be a symbolic link")
    if staging.exists():
        if not source_matches(source, metadata):
            raise FileExistsError("refusing unrecognized partial staging")
    else:
        staging.mkdir(mode=0o700)
        snapshot.write_json(source, metadata)
    cached_archive(metadata, cache, progress, reserve)
    # A hard kill can leave one of our atomic-write temporary files behind.
    # Only remove files in our reserved namespace after source verification.
    for path in staging.rglob(".snapshot-restore-*"):
        if path.is_symlink() or not path.is_file():
            raise ValueError("unexpected temporary path in staging")
        path.unlink()
    seen = set()
    extracted = 0
    reported = 0
    # ZstdFile bounds decompressed reads; tarfile's direct zstd stream can
    # materialize a whole compressed chunk's expansion before slicing it.
    with cache.open("rb") as compressed, ZstdFile(compressed) as uncompressed, tarfile.open(
            fileobj=uncompressed, mode="r|", bufsize=1024 * 1024, stream=True) as archive:
        for member in archive:
            relative = PurePosixPath(member.name)
            if (relative.is_absolute() or ".." in relative.parts or not relative.parts
                    or relative.parts[0] != "state" or not (member.isfile() or member.isdir())
                    or any(part.startswith(".snapshot-restore-") for part in relative.parts)):
                raise ValueError("snapshot contains an unexpected path, link or special file")
            if relative in seen:
                raise ValueError("snapshot contains a duplicate member")
            seen.add(relative)
            destination = staging.joinpath(*relative.parts)
            # Never follow pre-existing links in a recoverable staging directory.
            for path in (destination, *destination.parents):
                if path == staging.parent:
                    break
                if path.is_symlink():
                    raise ValueError("staging contains a symbolic link")
            if member.isdir():
                destination.mkdir(mode=0o700, parents=True, exist_ok=True)
                continue
            if shutil.disk_usage(target.parent).free < reserve + member.size:
                raise OSError("snapshot member would consume reserved free disk space")
            destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            temporary = None
            try:
                with archive.extractfile(member) as src, tempfile.NamedTemporaryFile(
                        dir=destination.parent, prefix=".snapshot-restore-", delete=False) as dst:
                    temporary = Path(dst.name)
                    pending = 0
                    while chunk := src.read(CHUNK):
                        dst.write(chunk)
                        pending += len(chunk)
                        if pending >= 16 * 1024 * 1024:
                            release_cache(dst, writing=True)
                            pending = 0
                    release_cache(dst, writing=True)
                if temporary.stat().st_size != member.size:
                    raise ValueError("snapshot member is incomplete")
                temporary.replace(destination)
            finally:
                if temporary is not None:
                    temporary.unlink(missing_ok=True)
            extracted += member.size
            release_cache(compressed)
            now = time.monotonic()
            if now - reported >= 15:
                reported = now
                snapshot.write_json(progress, {"phase": "extracting-verified-archive",
                    "extracted_bytes": extracted, "last_member": str(relative),
                    "free_gib": round(shutil.disk_usage(target.parent).free / 2**30, 1)})
    for path in staging.rglob("*"):
        if path == source:
            continue
        if path.is_symlink() or (path.is_file() and PurePosixPath(path.relative_to(staging)) not in seen):
            raise ValueError("unexpected extra data in staging; preserved for diagnosis")
    if not (staging / "state" / f"v{metadata['db_major']}" / "mainnet/CURRENT").is_file():
        raise ValueError("verified archive has no expected mainnet database")
    snapshot.write_json(staging / "snapshot-verified.json", metadata)
    staging.replace(target)
    snapshot.write_json(progress, {"phase": "verified", "height": metadata["height"],
        "sha256": metadata["sha256"], "compressed_bytes": metadata["size_bytes"],
        "extracted_bytes": extracted})
    cache.unlink()  # Only our verified cached archive; state is already promoted.
    print(f"Verified snapshot activated at {target}; temporary archive removed", flush=True)


if __name__ == "__main__":
    metadata_path, target, progress, cache = map(Path, sys.argv[1:5])
    metadata = json.loads(metadata_path.read_text())
    if metadata.get("network") != "mainnet" or metadata.get("snapshot_kind") != "archive":
        raise ValueError("this bootstrap requires a mainnet archive snapshot")
    try:
        restore(metadata, target, cache, progress, 60 * 2**30)
    except Exception as error:
        snapshot.write_json(progress, {"phase": "failed", "error": str(error)})
        raise
