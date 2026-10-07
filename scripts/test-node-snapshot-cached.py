"""Exercise process-resumable snapshot recovery with a small synthetic archive."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("cached", Path(__file__).with_name("node-snapshot-cached.py"))
cached = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cached)


class Response(io.BytesIO):
    def __init__(self, data, status=200, headers=None):
        super().__init__(data)
        self.status = status
        self.headers = headers or {"Content-Length": str(len(data)), "ETag": '"fixture"'}


class CachedTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.target = self.root / "node"
        self.staging = self.root / "node.partial"
        self.cache = self.root / "snapshot.tar.zst"
        self.progress = self.root / "progress.json"
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w:zst") as archive:
            member = tarfile.TarInfo("state/v28/mainnet/CURRENT")
            member.size = 8
            archive.addfile(member, io.BytesIO(b"fixture\n"))
        self.data = stream.getvalue()
        self.meta = {"url": "https://fixture.invalid/archive", "size_bytes": len(self.data),
                     "sha256": hashlib.sha256(self.data).hexdigest(), "height": 123, "db_major": 28}

    def owned_staging(self):
        self.staging.mkdir()
        (self.staging / ".snapshot-source.json").write_text(json.dumps(self.meta))
        current = self.staging / "state/v28/mainnet/CURRENT"
        current.parent.mkdir(parents=True)
        current.write_bytes(b"old interrupted content")
        return current

    def owned_cache(self, complete=False, prefix=9):
        (self.root / "snapshot.tar.zst.source.json").write_text(json.dumps(self.meta))
        p = self.cache if complete else self.root / "snapshot.tar.zst.partial"
        p.write_bytes(self.data if complete else self.data[:prefix])

    def restore(self):
        cached.restore(self.meta, self.target, self.cache, self.progress, 0)

    def test_process_restart_resumes_archive_and_restores_owned_partial(self):
        self.owned_staging()
        self.owned_cache()
        response = Response(self.data[9:], 206, {"Content-Range": f"bytes 9-{len(self.data)-1}/{len(self.data)}"})
        with patch.object(cached.snapshot.urllib.request, "urlopen", return_value=response) as request:
            self.restore()
        self.assertEqual(request.call_args.args[0].get_header("Range"), "bytes=9-")
        self.assertEqual((self.target / "state/v28/mainnet/CURRENT").read_bytes(), b"fixture\n")
        self.assertFalse(self.cache.exists())
        self.assertFalse(self.staging.exists())
        self.assertEqual(json.loads(self.progress.read_text())["phase"], "verified")

    def test_complete_cache_avoids_network_and_recovers_atomic_write_debris(self):
        current = self.owned_staging()
        (current.parent / ".snapshot-restore-interrupted").write_bytes(b"partial write")
        self.owned_cache(complete=True)
        with patch.object(cached.snapshot.urllib.request, "urlopen", side_effect=AssertionError("redownload")):
            self.restore()
            self.restore()  # Already promoted is also idempotent.
        self.assertEqual((self.target / "state/v28/mainnet/CURRENT").read_bytes(), b"fixture\n")

    def test_digest_failure_preserves_staging_and_archive(self):
        current = self.owned_staging()
        self.owned_cache(complete=True)
        self.cache.write_bytes(b"x" * len(self.data))
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            self.restore()
        self.assertEqual(current.read_bytes(), b"old interrupted content")
        self.assertTrue(self.cache.exists())
        self.assertFalse(self.target.exists())

    def test_unknown_partial_is_never_overwritten(self):
        self.staging.mkdir()
        with patch.object(cached.snapshot.urllib.request, "urlopen", side_effect=AssertionError("download")):
            with self.assertRaisesRegex(FileExistsError, "unrecognized"):
                self.restore()

    def test_extra_data_prevents_promotion_and_is_preserved(self):
        self.owned_staging()
        extra = self.staging / "keep"
        extra.write_text("unrecognized")
        self.owned_cache(complete=True)
        with self.assertRaisesRegex(ValueError, "extra data"):
            self.restore()
        self.assertEqual(extra.read_text(), "unrecognized")
        self.assertTrue(self.cache.exists())
        self.assertFalse(self.target.exists())

    def test_existing_staging_symlink_cannot_write_outside(self):
        self.staging.mkdir()
        (self.staging / ".snapshot-source.json").write_text(json.dumps(self.meta))
        outside = self.root / "outside"
        outside.mkdir()
        (self.staging / "state").symlink_to(outside, target_is_directory=True)
        self.owned_cache(complete=True)
        with self.assertRaisesRegex(ValueError, "symbolic link"):
            self.restore()
        self.assertEqual(list(outside.iterdir()), [])

    def test_reserved_space_blocks_download_without_touching_network(self):
        with patch.object(cached.snapshot.urllib.request, "urlopen", side_effect=AssertionError("download")):
            with self.assertRaisesRegex(OSError, "reserved free disk"):
                cached.cached_archive(self.meta, self.cache, self.progress, 2**70)

    def test_wrong_source_cannot_resume_download(self):
        self.owned_cache()
        (self.root / "snapshot.tar.zst.source.json").write_text('{}')
        with self.assertRaisesRegex(ValueError, "matching pinned source"):
            cached.cached_archive(self.meta, self.cache, self.progress, 0)


if __name__ == "__main__":
    unittest.main()
