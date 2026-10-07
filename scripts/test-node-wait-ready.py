"""Offline startup-gate checks; credentials are synthetic and no node is needed."""
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ready", Path(__file__).with_name("node-wait-ready.py"))
ready = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ready)


class Response(io.BytesIO):
    status = 200


class ReadinessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "auth").mkdir()
        (self.root / "auth/.cookie").write_text("fixture:synthetic-password")

    def reply(self, **overrides):
        info = {"chain": "main", "blocks": 123, "estimatedheight": 124}
        info.update(overrides)
        return Response(json.dumps({"result": info, "error": None}).encode())

    def test_recent_mainnet_and_ready_endpoint_allow_start(self):
        with patch.object(ready.urllib.request, "urlopen", side_effect=[self.reply(), Response()]) as request:
            self.assertEqual(ready.check_ready(self.root, 123),
                             (True, {"phase": "validator-ready", "height": 123}))
            self.assertEqual(request.call_count, 2)
            self.assertTrue(request.call_args_list[0].args[0].has_header("Authorization"))

    def test_behind_or_pre_snapshot_node_cannot_start_indexer(self):
        for info in ({"blocks": 122}, {"estimatedheight": 126}):
            with self.subTest(info=info), patch.object(ready.urllib.request, "urlopen", return_value=self.reply(**info)) as request:
                self.assertFalse(ready.check_ready(self.root, 123)[0])
                self.assertEqual(request.call_count, 1)

    def test_ready_endpoint_must_pass_even_when_rpc_height_is_current(self):
        unavailable = urllib.error.HTTPError("http://127.0.0.1:8234/ready", 503, "catching up", {}, None)
        with patch.object(ready.urllib.request, "urlopen", side_effect=[self.reply(), unavailable]):
            self.assertFalse(ready.check_ready(self.root, 123)[0])

    def test_wrong_network_fails_closed(self):
        with patch.object(ready.urllib.request, "urlopen", return_value=self.reply(chain="test")):
            with self.assertRaisesRegex(ValueError, "not mainnet"):
                ready.check_ready(self.root, 123)

    def test_missing_cookie_waits_without_network_request(self):
        (self.root / "auth/.cookie").unlink()
        with patch.object(ready.urllib.request, "urlopen", side_effect=AssertionError("unauthenticated request")):
            self.assertFalse(ready.check_ready(self.root, 123)[0])


if __name__ == "__main__":
    unittest.main()
