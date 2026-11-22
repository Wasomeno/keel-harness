#!/usr/bin/env python3
"""Restricted HTTP relay for the SWE-bench agent container.

The agent container is attached to an internal Docker network with no route
off the host.  This relay is the only other container on that network and the
only process allowed to reach the provider.  It accepts origin-form HTTP
requests, forwards them to one configured upstream origin, and rejects
anything outside the configured path prefix.  The upstream host is never taken
from the request, so the agent cannot turn the relay into an open proxy.

The script is copied into a container and run with the image's Python.  Keep
it compatible with Python 3.6, the oldest interpreter in the SWE-bench images.
"""

import argparse
import http.client
import ssl
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn
from urllib.parse import urlsplit

# A rejected request must have its body consumed before the connection closes;
# otherwise close() can send an RST and hide the denial from the client.  Both
# the byte cap and the timeout bound a hostile or stalled sender.
MAX_DRAIN_BYTES = 1024 * 1024
DRAIN_TIMEOUT_SECONDS = 0.5


class Upstream(object):
    def __init__(self, url):
        parsed = urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise ValueError("upstream must be an absolute http(s) URL")
        self.scheme = parsed.scheme
        self.host = parsed.hostname
        self.port = parsed.port or (443 if parsed.scheme == "https" else 80)
        self.netloc = parsed.netloc
        self.path_prefix = parsed.path.rstrip("/") or "/"


def path_allowed(request_path, prefix):
    """Whether an origin-form request path is under the configured prefix."""
    if not request_path.startswith("/") or request_path.startswith("//"):
        return False
    # A dot segment or an encoded separator can make the upstream resolve a
    # path the prefix check did not see.  Reject rather than rewrite it.
    if (
        "\\" in request_path
        or "%2e" in request_path.lower()
        or "%2f" in request_path.lower()
        or "%5c" in request_path.lower()
    ):
        return False
    if any(segment in (".", "..") for segment in request_path.split("/")):
        return False
    if prefix == "/":
        return True
    return request_path == prefix or request_path.startswith(prefix + "/")


def log_line(method, path, status, size):
    """One controlled relay line: method, path, status and body bytes only."""
    return "relay: %s %s %s %s" % (method, path, status, size)


class RelayHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    upstream = None

    def log_message(self, fmt, *args):
        # Only fixed handler messages reach here; request headers and bodies
        # never do.  Keep the same prefix so the log stays greppable.
        sys.stderr.write("relay: " + (fmt % args) + "\n")
        sys.stderr.flush()

    def log_request(self, code="-", size="-"):
        # BaseHTTPRequestHandler calls this before the response body is read.
        # _log writes the one line once the outcome and byte count are known.
        pass

    def _log(self, status, size):
        # urlsplit drops any query string, which can carry credentials for
        # APIs that put keys there instead of in a header.
        path = urlsplit(self.path).path
        sys.stderr.write(log_line(self.command, path, status, size) + "\n")
        sys.stderr.flush()

    def do_GET(self):
        self._forward()

    def do_HEAD(self):
        self._forward()

    def do_POST(self):
        self._forward()

    def _drain_request_body(self):
        """Read a rejected request's body so close() sends FIN, not RST.

        A client that sent a body and is waiting for the response can otherwise
        see ECONNRESET instead of the denial.  The drain is capped and
        time-limited so a hostile Content-Length or a stalled sender cannot
        keep the handler reading forever.
        """
        chunked = self.headers.get("Transfer-Encoding", "").lower() == "chunked"
        if chunked:
            limit = MAX_DRAIN_BYTES
        else:
            try:
                length = int(self.headers.get("Content-Length") or 0)
            except (TypeError, ValueError):
                return
            if length <= 0:
                return
            if length > MAX_DRAIN_BYTES:
                # The declared body is larger than the drain cap.  Do not read
                # it; the client may see a reset instead of the denial, which
                # is acceptable for this pathological case.
                return
            limit = length

        try:
            self.connection.settimeout(DRAIN_TIMEOUT_SECONDS)
            remaining = limit
            while remaining > 0:
                chunk = self.rfile.read1(min(remaining, 65536))
                if not chunk:
                    break
                remaining -= len(chunk)
        except (OSError, ValueError):
            pass
        finally:
            try:
                self.connection.settimeout(None)
            except OSError:
                pass

    def _deny(self, status, reason, *, drain_body=True):
        if drain_body:
            self._drain_request_body()
        body = (reason + "\n").encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True
        self._log(status, len(body))

    def _forward(self):
        split = urlsplit(self.path)
        if not path_allowed(split.path, self.upstream.path_prefix):
            self._deny(403, "path is not under the allowed prefix")
            return
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            self._deny(411, "chunked request bodies are not supported")
            return
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {}
        for name, value in self.headers.items():
            if name.lower() in (
                "host",
                "connection",
                "proxy-connection",
                "content-length",
                "transfer-encoding",
            ):
                continue
            headers[name] = value
        headers["Host"] = self.upstream.netloc
        if body is not None:
            headers["Content-Length"] = str(len(body))

        if self.upstream.scheme == "https":
            connection = http.client.HTTPSConnection(
                self.upstream.host,
                self.upstream.port,
                context=ssl.create_default_context(),
                timeout=600,
            )
        else:
            connection = http.client.HTTPConnection(
                self.upstream.host, self.upstream.port, timeout=600
            )
        started = False
        sent = 0
        try:
            connection.request(self.command, self.path, body=body, headers=headers)
            response = connection.getresponse()
            started = True
            self.send_response(response.status, response.reason)
            for name, value in response.getheaders():
                if name.lower() in (
                    "transfer-encoding",
                    "connection",
                    "content-length",
                    "keep-alive",
                    "proxy-authenticate",
                    "proxy-authorization",
                    "te",
                    "trailer",
                    "upgrade",
                ):
                    continue
                self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            while True:
                chunk = response.read(4096)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
                sent += len(chunk)
            self._log(response.status, sent)
        except Exception as error:  # noqa: BLE001 - report and close the connection
            # The exception type is safe; its text can echo request details.
            self.log_message("upstream error: %s", type(error).__name__)
            if started:
                self._log(502, sent)
            else:
                try:
                    self._deny(502, "upstream relay failed", drain_body=False)
                except Exception:
                    pass
        finally:
            connection.close()
            self.close_connection = True


class ThreadedServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def build_server(listen, upstream):
    host, separator, port = listen.rpartition(":")
    if not separator:
        raise ValueError("--listen must look like HOST:PORT")
    RelayHandler.upstream = Upstream(upstream)
    return ThreadedServer((host or "0.0.0.0", int(port)), RelayHandler)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", default="0.0.0.0:8080")
    parser.add_argument("--upstream", required=True)
    args = parser.parse_args(argv)
    build_server(args.listen, args.upstream).serve_forever()


if __name__ == "__main__":
    main()
