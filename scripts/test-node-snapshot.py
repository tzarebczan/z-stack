"""Offline bootstrap safety checks; no chain data or network is used."""
import hashlib
import http.client
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("snapshot", Path(__file__).with_name("node-snapshot.py"))
snapshot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(snapshot)


class Response(io.BytesIO):
    def __init__(self, data, status=200, headers=None):
        super().__init__(data)
        self.status = status
        self.headers = headers or {"Content-Length": str(len(data)), "ETag": '"fixture"'}


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.target, self.progress = self.root / "node", self.root / "progress.json"

    def fixture(self, name="state/v28/mainnet/CURRENT"):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w:zst") as archive:
            member = tarfile.TarInfo(name)
            member.size = 8
            archive.addfile(member, io.BytesIO(b"fixture\n"))
        data = stream.getvalue()
        metadata = {"url": "https://fixture.invalid/archive", "size_bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest(), "db_major": 28, "height": 123}
        return data, metadata

    def run_extract(self, data, metadata, reserve=0):
        with patch.object(snapshot.urllib.request, "urlopen", return_value=Response(data)):
            snapshot.extract(metadata, self.target, self.progress, reserve)

    def test_verified_activation_and_idempotent_restart(self):
        data, meta = self.fixture()
        self.run_extract(data, meta)
        self.assertEqual((self.target / "state/v28/mainnet/CURRENT").read_bytes(), b"fixture\n")
        with patch.object(snapshot.urllib.request, "urlopen", side_effect=AssertionError("redownload")):
            snapshot.extract(meta, self.target, self.progress, 0)
        self.assertEqual(json.loads(self.progress.read_text())["phase"], "verified")

    def test_wrong_digest_never_activates(self):
        data, meta = self.fixture()
        meta["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            self.run_extract(data, meta)
        self.assertFalse(self.target.exists())
        self.assertTrue((self.root / "node.partial").exists())

    def test_path_traversal_never_writes_outside_staging(self):
        data, meta = self.fixture("../escaped")
        with self.assertRaisesRegex(ValueError, "unexpected snapshot member"):
            self.run_extract(data, meta)
        self.assertFalse((self.root / "escaped").exists())
        self.assertFalse(self.target.exists())

    def test_existing_data_and_partial_staging_are_preserved(self):
        data, meta = self.fixture()
        partial = self.root / "node.partial"
        partial.mkdir()
        (partial / "keep").write_text("existing")
        with self.assertRaises(FileExistsError):
            self.run_extract(data, meta)
        self.assertEqual((partial / "keep").read_text(), "existing")
        self.target.mkdir()
        with self.assertRaises(FileExistsError):
            self.run_extract(data, meta)

    def test_reserved_free_space_is_enforced(self):
        data, meta = self.fixture()
        with self.assertRaisesRegex(OSError, "reserved free disk"):
            self.run_extract(data, meta, reserve=2**70)
        self.assertFalse(self.target.exists())

    def test_resume_requires_exact_range_and_hashes_partial_read_once(self):
        data, meta = self.fixture()
        reader = snapshot.Download(meta, self.progress, self.root, 0)
        first = Response(data)
        first.read = lambda _: (_ for _ in ()).throw(http.client.IncompleteRead(data[:9]))
        second = Response(data[9:], 206, {"Content-Range": f"bytes 9-{len(data)-1}/{len(data)}", "ETag": '"fixture"'})
        with patch.object(snapshot.urllib.request, "urlopen", side_effect=[first, second]):
            self.assertEqual(reader.read(100), data[:9])
            self.assertEqual(reader.read(len(data)), data[9:])
        self.assertEqual(reader.digest.hexdigest(), meta["sha256"])
        reader.close()
        wrong = snapshot.Download(meta, self.progress, self.root, 0)
        wrong.offset = 9
        with patch.object(snapshot.urllib.request, "urlopen", return_value=Response(data)):
            with self.assertRaisesRegex(ValueError, "pinned byte range"):
                wrong.read(100)


if __name__ == "__main__":
    unittest.main()
