#!/usr/bin/env python3
"""Stream a pinned Zakura archive into staging; activate only after SHA-256 verification.

Requires Python 3.14 (tarfile zstd). No compressed archive is retained. HTTP
interruptions resume within this process; a process restart preserves partial
staging for inspection and refuses to overwrite it.
"""
import hashlib
import http.client
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile
import time
import urllib.error
import urllib.request


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


class Download:
    def __init__(self, metadata, progress, disk, reserve):
        self.metadata, self.progress, self.disk, self.reserve = metadata, progress, disk, reserve
        self.offset = 0
        self.digest = hashlib.sha256()
        self.response = None
        self.etag = None
        self.started = time.monotonic()
        self.initial_offset = 0
        self.phase = "downloading"
        self.reported = 0
        self.retries = 0

    def connect(self):
        headers = {"User-Agent": "curl/8.19.0", "Accept-Encoding": "identity"}
        if self.offset:
            headers["Range"] = f"bytes={self.offset}-"
            if self.etag:
                headers["If-Match"] = self.etag
        response = urllib.request.urlopen(urllib.request.Request(self.metadata["url"], headers=headers), timeout=60)
        total = self.metadata["size_bytes"]
        if self.offset:
            expected = f"bytes {self.offset}-{total - 1}/{total}"
            if response.status != 206 or response.headers.get("Content-Range") != expected:
                response.close()
                raise ValueError("snapshot resume response does not match the pinned byte range")
        elif response.status != 200 or int(response.headers.get("Content-Length", -1)) != total:
            response.close()
            raise ValueError("snapshot response size differs from the pinned manifest")
        etag = response.headers.get("ETag")
        if self.etag and self.etag != etag:
            response.close()
            raise ValueError("snapshot changed during transfer")
        self.etag = etag
        self.response = response

    def read(self, size):
        if self.offset == self.metadata["size_bytes"]:
            return b""
        size = min(size, 4 * 1024 * 1024, self.metadata["size_bytes"] - self.offset)
        while True:
            if shutil.disk_usage(self.disk).free < self.reserve:
                raise OSError("snapshot extraction stopped to preserve reserved free disk space")
            try:
                if self.response is None:
                    self.connect()
                try:
                    chunk = self.response.read(size)
                except http.client.IncompleteRead as error:
                    chunk = error.partial
                    self.response.close()
                    self.response = None
                if not chunk:
                    raise ConnectionError("snapshot connection ended before the pinned size")
                break
            except (OSError, http.client.HTTPException, urllib.error.URLError) as error:
                if self.response:
                    self.response.close()
                    self.response = None
                self.retries += 1
                if self.retries > 8:
                    raise RuntimeError("snapshot download exceeded consecutive retry limit") from error
                print(f"Retry {self.retries} at byte {self.offset}: {type(error).__name__}", flush=True)
                time.sleep(min(2 ** self.retries, 30))
        self.retries = 0
        self.offset += len(chunk)
        self.digest.update(chunk)
        now = time.monotonic()
        if now - self.reported >= 15:
            self.reported = now
            result = {"phase": self.phase, "compressed_bytes": self.offset,
                      "total_bytes": self.metadata["size_bytes"],
                      "percent": round(100 * self.offset / self.metadata["size_bytes"], 2),
                      "average_mib_s": round((self.offset - self.initial_offset) / max(now - self.started, .001) / 2**20, 2),
                      "free_gib": round(shutil.disk_usage(self.disk).free / 2**30, 1)}
            write_json(self.progress, result)
            print(json.dumps(result), flush=True)
        return chunk

    def close(self):
        if self.response:
            self.response.close()


def extract(metadata, target, progress, reserve):
    marker = target / "snapshot-verified.json"
    if target.exists():
        if marker.is_file() and json.loads(marker.read_text()).get("sha256") == metadata["sha256"]:
            return
        raise FileExistsError("refusing to replace an existing node directory")
    staging = target.with_name(target.name + ".partial")
    staging.mkdir(mode=0o700)  # Refuse a prior partial extraction; never silently erase it.
    reader = Download(metadata, progress, target.parent, reserve)
    try:
        with tarfile.open(fileobj=reader, mode="r|zst", bufsize=4 * 1024 * 1024) as archive:
            for member in archive:
                path = PurePosixPath(member.name)
                if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "state":
                    raise ValueError(f"unexpected snapshot member: {member.name}")
                if not (member.isfile() or member.isdir()):
                    raise ValueError("snapshot contains a link or special file")
                if shutil.disk_usage(target.parent).free < reserve + member.size:
                    raise OSError("snapshot member would consume reserved free disk space")
                member.mode = 0o700 if member.isdir() else 0o600
                archive.extract(member, staging, filter="data")
        # Tar EOF can precede the end of the compressed stream. Hash every byte.
        while reader.read(4 * 1024 * 1024):
            pass
        if reader.offset != metadata["size_bytes"] or reader.digest.hexdigest() != metadata["sha256"]:
            raise ValueError("snapshot SHA-256 or size mismatch; staging was not activated")
        database = staging / "state" / f"v{metadata['db_major']}" / "mainnet"
        if not (database / "CURRENT").is_file():
            raise ValueError("verified archive has no expected mainnet RocksDB CURRENT file")
        write_json(staging / "snapshot-verified.json", metadata)
        # The node service starts only after this process succeeds.
        os.rename(staging, target)
        write_json(progress, {"phase": "verified", "height": metadata["height"],
                              "sha256": metadata["sha256"], "compressed_bytes": reader.offset})
        print(f"Verified snapshot activated at {target}", flush=True)
    finally:
        reader.close()


if __name__ == "__main__":
    metadata_path, target_path, progress_path = map(Path, sys.argv[1:4])
    metadata = json.loads(metadata_path.read_text())
    if metadata.get("network") != "mainnet" or metadata.get("snapshot_kind") != "archive":
        raise ValueError("this bootstrap requires a mainnet archive snapshot")
    try:
        extract(metadata, target_path, progress_path, 60 * 2**30)
    except Exception as error:
        write_json(progress_path, {"phase": "failed", "error": str(error)})
        raise
