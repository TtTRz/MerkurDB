#!/usr/bin/env python3
"""Exercise MCP against real merkur-server processes; no model API calls."""
import argparse
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from contextlib import ExitStack

TOKEN = "mcp-e2e-token"


class Mcp:
    def __init__(self, binary, url, namespace, token=TOKEN):
        env = os.environ.copy()
        for key in ("MERKUR_DB_PATH", "MERKUR_EMBED_DIM"):
            env.pop(key, None)
        env.update(MERKUR_URL=url, MERKUR_TOKEN=token, MERKUR_NAMESPACE=namespace)
        self.process = subprocess.Popen(
            [str(binary)], env=env, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        self.buffer = b""
        self.sequence = 0
        try:
            self.request("initialize", {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "real-server-test", "version": "1"},
            })
            self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        except BaseException:
            self.close()
            raise

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdin.close()
        self.process.stdout.close()

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if b"\n" not in self.buffer:
                ready, _, _ = select.select([self.process.stdout], [], [], max(0, deadline-time.monotonic()))
                if not ready:
                    break
                data = os.read(self.process.stdout.fileno(), 65536)
                assert data, "MCP exited before responding"
                self.buffer += data
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                response = json.loads(line)
                if response.get("id") == self.sequence:
                    assert "error" not in response, response
                    return response["result"]
        raise AssertionError("MCP request timed out")

    def call(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        assert not result.get("isError"), result
        return json.loads(result["content"][0]["text"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", default="target/debug/merkur-server", type=Path)
    parser.add_argument("--mcp", default="target/debug/merkur-mcp", type=Path)
    args = parser.parse_args()
    server_binary, mcp_binary = args.server.resolve(), args.mcp.resolve()
    assert server_binary.is_file() and mcp_binary.is_file(), "Build merkur-server and merkur-mcp first"
    Path("target").mkdir(exist_ok=True)
    with ExitStack() as stack:
        directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="mcp-e2e-", dir="target"))).resolve()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = directory / "config.yaml"
        config.write_text(f"""
server:
  host: "127.0.0.1"
  port: {port}
  dev_mode: false
storage:
  type: sqlite
  sqlite:
    path: "{directory / 'memory.db'}"
plugins:
  embedder:
    type: noop
  consolidator:
    type: noop
auth:
  disabled: false
  tokens: ["{TOKEN}"]
consolidation:
  interval_seconds: 86400
  adjudication_candidates: 0
forgetting:
  interval_seconds: 86400
write:
  dedup_enabled: true
  dedup_threshold: 0.99
rate_limit:
  enabled: false
""")
        log = stack.enter_context((directory / "server.log").open("w"))
        env = {k: v for k, v in os.environ.items() if not k.startswith("MERKUR_")}
        server = subprocess.Popen([str(server_binary), "--config", str(config)], env=env, stdout=log, stderr=log)
        def stop_server():
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
        stack.callback(stop_server)
        url = f"http://127.0.0.1:{port}"
        deadline = time.monotonic() + 15
        while True:
            try:
                with urllib.request.urlopen(url + "/v1/health", timeout=1) as response:
                    assert response.status == 200
                break
            except (OSError, urllib.error.URLError):
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError("Server did not become healthy")
                time.sleep(0.1)
        clients = []
        for namespace, token in [("alpha", TOKEN), ("beta", TOKEN), ("alpha", "invalid-token")]:
            client = Mcp(mcp_binary, url, namespace, token)
            stack.callback(client.close)
            clients.append(client)
        alpha, beta, denied = clients
        content = "Release the application on Friday after approval."
        first = alpha.call("write_memory", {"content": content, "context": {"session_id": "session-1"}})
        duplicate = alpha.call("write_memory", {"content": content})
        assert duplicate["id"] == first["id"], "HTTP write governance did not deduplicate"
        foreign = beta.call("write_memory", {"content": content})
        assert foreign["id"] != first["id"], "Dedup crossed namespace boundary"
        original = alpha.call("get_memory", {"id": first["id"]})
        assert original["context"]["session_id"] == "session-1"
        count = original["access_count"]
        assembled = alpha.call("assemble_context", {"query": content, "token_budget": 100, "limit": 10})
        assert assembled["token_estimate"] <= 100
        assert [item["id"] for item in assembled["items"]] == [first["id"]]
        assert assembled["items"][0]["namespace"] == "alpha"
        assert content in assembled["digest"]
        assert alpha.call("get_memory", {"id": first["id"]})["access_count"] == count + 1
        assert beta.call("get_memory", {"id": foreign["id"]})["access_count"] == 0
        hits = alpha.call("search_memory", {"query": content})
        assert [item["id"] for item in hits] == [first["id"]]
        rejected = denied.request("tools/call", {"name": "assemble_context", "arguments": {"query": content, "token_budget": 100}})
        assert rejected.get("isError") is True
        assert "UNAUTHORIZED" in rejected["content"][0]["text"]
        print("PASS: real MCP → HTTP write/dedup/context/source/access/namespace/auth")


if __name__ == "__main__":
    main()
