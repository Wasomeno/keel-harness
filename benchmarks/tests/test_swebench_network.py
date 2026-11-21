"""Docker-gated check of the restricted network's only reachable peer.

This test runs only where Docker is available and the small relay image is
already present.  It starts a fake provider on Docker's default bridge, lets
the real runner create the internal network and relay, and then checks that an
agent container on that network cannot reach GitHub but can reach the relay.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks" / "swebench"))

import run_keel as runner  # noqa: E402

IMAGE = os.environ.get("KEEL_SWEBENCH_RELAY_IMAGE", "python:3.11-alpine")

FAKE_PROVIDER = """\
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(length)
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

HTTPServer(("0.0.0.0", 9000), Handler).serve_forever()
"""


def docker_ready() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        return (
            subprocess.run(
                ["docker", "image", "inspect", IMAGE],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=30,
            ).returncode
            == 0
        )
    except (OSError, subprocess.SubprocessError):
        return False


def docker(*args: str, timeout: int = 120) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["docker", *args], capture_output=True, text=True, timeout=timeout
    )


@unittest.skipUnless(
    docker_ready(), f"needs Docker and the local image {IMAGE}"
)
class NetworkIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.suffix = uuid.uuid4().hex[:8]
        self.agent = f"keel-net-agent-{self.suffix}"
        self.provider = f"keel-net-provider-{self.suffix}"
        self.containers = [self.agent, self.provider]
        self.network = None
        self.addCleanup(self.cleanup)
        self.docker = runner.Docker(runner.SubprocessRunner(), "")

    def cleanup(self) -> None:
        for name in self.containers:
            docker("rm", "-f", name)
        if self.network is not None:
            runner.cleanup_network(self.docker, self.network, keep=False)

    def start_provider(self) -> str:
        script = self.root / "fake_provider.py"
        script.write_text(FAKE_PROVIDER, encoding="utf-8")
        self.assertEqual(docker("run", "-d", "--name", self.provider, IMAGE,
                                "sleep", "infinity").returncode, 0)
        self.assertEqual(
            docker("cp", str(script), f"{self.provider}:/tmp/fake_provider.py").returncode,
            0,
        )
        self.assertEqual(
            docker("exec", "-d", self.provider, "python3", "/tmp/fake_provider.py").returncode,
            0,
        )
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            probe = docker(
                "exec",
                self.provider,
                "python3",
                "-c",
                "import socket; socket.create_connection(('127.0.0.1', 9000), 2)",
            )
            if probe.returncode == 0:
                inspected = docker("inspect", self.provider)
                self.assertEqual(inspected.returncode, 0)
                address = json.loads(inspected.stdout)[0]["NetworkSettings"][
                    "Networks"
                ]["bridge"]["IPAddress"]
                self.assertTrue(address)
                return address
            time.sleep(0.2)
        self.fail("the fake provider did not start")

    def test_github_is_unreachable_but_the_relay_forwards(self) -> None:
        provider_ip = self.start_provider()
        config = runner.Config(
            keel_binary=Path("/unused"),
            keel_target="neuralwatt/deepseek-v4.1-flash",
            model_label="test",
            max_turns=1,
            timeout=30,
            output_dir=self.root,
            run_id="net-test",
            image_template=runner.DEFAULT_IMAGE_TEMPLATE,
            patch_cap_bytes=runner.DEFAULT_PATCH_CAP,
            platform="",
            keep_container=False,
            base_url=f"http://{provider_ip}:9000/v1",
        )
        self.network = runner.prepare_network(config, self.docker, IMAGE)
        self.assertEqual(self.network.allowed_hosts, (provider_ip,))
        relay_name = self.network.relay_name

        started = self.docker.run_container(
            self.agent, IMAGE, network=self.network.network_name
        )
        self.assertEqual(started.returncode, 0, started.stderr)

        blocked = self.docker.exec(
            self.agent,
            ["wget", "-T", "5", "-qO-", "https://api.github.com"],
            timeout=30,
        )
        self.assertNotEqual(
            blocked.returncode, 0, "GitHub must be unreachable without egress"
        )
        self.assertNotIn("zen", blocked.stdout)

        no_route = self.docker.exec(
            self.agent,
            [
                "python3",
                "-c",
                "import socket; "
                "socket.create_connection(('1.1.1.1', 443), 3)",
            ],
            timeout=30,
        )
        self.assertNotEqual(no_route.returncode, 0, "there must be no default route")

        forwarded = self.docker.exec(
            self.agent,
            [
                "python3",
                "-c",
                "import urllib.request; "
                f"r = urllib.request.urlopen('http://{relay_name}:8080/v1/chat', "
                "data=b'{}', timeout=5); "
                "print(r.status); print(r.read().decode())",
            ],
            timeout=30,
        )
        self.assertEqual(forwarded.returncode, 0, forwarded.stderr)
        self.assertIn("200", forwarded.stdout)
        self.assertIn('{"ok":true}', forwarded.stdout)


if __name__ == "__main__":
    unittest.main()
