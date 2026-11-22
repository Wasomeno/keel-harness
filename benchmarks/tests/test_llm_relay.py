"""Relay tests that do not need Docker.

The relay is the only peer on the agent's internal network, so these tests
exercise its two safety properties directly: one exact origin with a path
prefix, and a log that never carries credentials or payload bytes.
"""

import contextlib
import http.client
import io
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks" / "swebench"))

import llm_relay  # noqa: E402


class UpstreamHandler(BaseHTTPRequestHandler):
    requests: list[tuple[str, dict, bytes]] = []

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        type(self).requests.append((self.path, dict(self.headers), body))
        payload = b"upstream-ok"
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, fmt, *args):
        pass


class RelayTests(unittest.TestCase):
    def setUp(self) -> None:
        UpstreamHandler.requests = []
        self.upstream = ThreadingHTTPServer(("127.0.0.1", 0), UpstreamHandler)
        self.upstream_thread = threading.Thread(
            target=self.upstream.serve_forever, daemon=True
        )
        self.upstream_thread.start()
        upstream_port = self.upstream.server_address[1]
        self.relay = llm_relay.build_server(
            "127.0.0.1:0", f"http://127.0.0.1:{upstream_port}/v1"
        )
        self.relay_thread = threading.Thread(
            target=self.relay.serve_forever, daemon=True
        )
        self.relay_thread.start()
        self.addCleanup(self.stop_servers)

    def stop_servers(self) -> None:
        for server, thread in (
            (self.relay, self.relay_thread),
            (self.upstream, self.upstream_thread),
        ):
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    def request(self, method: str, path: str, body: bytes, headers: dict):
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.relay.server_address[1], timeout=5
        )
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        payload = response.read()
        connection.close()
        return response.status, payload

    def read_log(self, stream: io.StringIO) -> str:
        for _ in range(100):
            text = stream.getvalue()
            if "relay:" in text:
                return text
            time.sleep(0.01)
        return stream.getvalue()

    def test_authorization_and_payload_never_reach_the_log(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status, payload = self.request(
                "POST",
                "/v1/chat/completions?api_key=query-secret",
                b'{"prompt":"payload-secret"}',
                {
                    "Authorization": "Bearer key-secret",
                    "Content-Type": "application/json",
                },
            )
            log = self.read_log(stderr)

        self.assertEqual(status, 200)
        self.assertEqual(payload, b"upstream-ok")
        self.assertEqual(log.strip(), "relay: POST /v1/chat/completions 200 11")
        for secret in (
            "key-secret",
            "payload-secret",
            "query-secret",
            "Authorization",
        ):
            self.assertNotIn(secret, log)

        self.assertEqual(len(UpstreamHandler.requests), 1)
        _, headers, body = UpstreamHandler.requests[0]
        self.assertEqual(headers.get("Authorization"), "Bearer key-secret")
        self.assertEqual(body, b'{"prompt":"payload-secret"}')

    def test_a_path_outside_the_prefix_is_rejected_and_logged(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status, payload = self.request(
                "POST",
                "/v2/chat/completions",
                b"{}",
                {"Authorization": "Bearer key-secret"},
            )
            log = self.read_log(stderr)

        self.assertEqual(status, 403)
        self.assertEqual(payload, b"path is not under the allowed prefix\n")
        self.assertEqual(
            log.strip(),
            f"relay: POST /v2/chat/completions 403 {len(payload)}",
        )
        self.assertNotIn("key-secret", log)
        self.assertEqual(UpstreamHandler.requests, [])

    def test_denied_requests_with_a_body_get_a_clean_403_every_time(self) -> None:
        # A denial that leaves the request body unread turns close() into an
        # RST, so the client can see a reset instead of the 403.  One request
        # fails only occasionally; the loop makes the regression deterministic.
        for iteration in range(20):
            with self.subTest(iteration=iteration), contextlib.redirect_stderr(
                io.StringIO()
            ):
                status, payload = self.request(
                    "POST",
                    "/v2/chat/completions",
                    b'{"prompt":"blocked"}',
                    {"Content-Type": "application/json"},
                )
            self.assertEqual(status, 403)
            self.assertEqual(payload, b"path is not under the allowed prefix\n")
        self.assertEqual(UpstreamHandler.requests, [])

    def test_a_dot_segment_is_rejected_before_it_reaches_the_origin(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            status, payload = self.request(
                "POST", "/v1/../v2/chat", b"{}", {"Content-Type": "application/json"}
            )
        self.assertEqual(status, 403)
        self.assertIn(b"path is not under the allowed prefix", payload)
        self.assertEqual(UpstreamHandler.requests, [])


if __name__ == "__main__":
    unittest.main()
