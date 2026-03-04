#!/usr/bin/env python3
"""
Smoke test for mish MCP server over stdio.

Drives `mish serve` with JSON-RPC requests, validates responses.
Run: python3 tests/smoke_mcp.py [path-to-mish-binary]
"""

import json
import os
import signal
import subprocess
import sys
import time

BINARY = sys.argv[1] if len(sys.argv) > 1 else "./target/release/mish"

# Colors
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
DIM = "\033[2m"
RESET = "\033[0m"
BOLD = "\033[1m"

passed = 0
failed = 0
errors = []


def ok(label, detail=""):
    global passed
    passed += 1
    detail_str = f" {DIM}{detail}{RESET}" if detail else ""
    print(f"  {GREEN}+{RESET} {label}{detail_str}")


def fail(label, detail=""):
    global failed
    failed += 1
    errors.append(f"{label}: {detail}")
    print(f"  {RED}!{RESET} {label} — {detail}")


def section(title):
    print(f"\n{BOLD}{title}{RESET}")


def extract_tool_text(resp):
    """Extract compact text from MCP content-wrapped response.

    tools/call responses use: result.content[0].text = compact text string.
    """
    content = resp.get("result", {}).get("content", [])
    if content and content[0].get("type") == "text":
        return content[0]["text"]
    return ""


class MishServer:
    def __init__(self):
        self.proc = subprocess.Popen(
            [BINARY, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={**os.environ, "SHELL": "/bin/bash"},
        )

    def request(self, method, params=None, req_id=1):
        msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        line = json.dumps(msg) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()

        resp_line = self.proc.stdout.readline().decode().strip()
        if not resp_line:
            return None
        return json.loads(resp_line)

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "id": None, "method": method}
        if params is not None:
            msg["params"] = params
        line = json.dumps(msg) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()
        time.sleep(0.05)

    def shutdown(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        return self.proc.returncode


def main():
    print(f"{BOLD}mish MCP smoke test{RESET}")
    print(f"Binary: {BINARY}")
    print(f"PID: ", end="")

    server = MishServer()
    print(f"{server.proc.pid}")

    # ── 1. Initialize ──
    section("1. MCP Handshake")

    resp = server.request("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "smoke-test", "version": "1.0"},
    })

    if resp and resp.get("result", {}).get("serverInfo", {}).get("name") == "mish":
        ok("initialize", f"protocol={resp['result']['protocolVersion']}")
    else:
        fail("initialize", str(resp))

    server.notify("notifications/initialized")
    ok("notifications/initialized")

    # ── 2. tools/list ──
    section("2. Tool Discovery")

    resp = server.request("tools/list", req_id=2)
    tools = resp.get("result", {}).get("tools", [])
    tool_names = sorted([t["name"] for t in tools])
    expected = ["sh_help", "sh_interact", "sh_run", "sh_session", "sh_spawn"]

    if tool_names == expected:
        ok("tools/list", f"{len(tools)} tools: {', '.join(tool_names)}")
    else:
        fail("tools/list", f"expected {expected}, got {tool_names}")

    for tool in tools:
        if tool.get("inputSchema", {}).get("type") == "object":
            ok(f"  schema:{tool['name']}", "has inputSchema")
        else:
            fail(f"  schema:{tool['name']}", "missing/invalid inputSchema")

    # ── 3. sh_help ──
    section("3. sh_help")

    resp = server.request("tools/call", {
        "name": "sh_help", "arguments": {}
    }, req_id=3)

    text = extract_tool_text(resp)
    if "sh_run" in text and "sh_spawn" in text:
        ok("sh_help", "contains tool descriptions")
    else:
        fail("sh_help", f"unexpected: {text[:100]}")

    # ── 4. sh_run — simple command ──
    section("4. sh_run")

    resp = server.request("tools/call", {
        "name": "sh_run",
        "arguments": {"cmd": "echo hello_from_mish", "timeout": 10}
    }, req_id=10)

    text = extract_tool_text(resp)
    if "exit:0" in text and "hello_from_mish" in text:
        ok("echo", "exit:0 + output present")
    else:
        fail("echo", text[:200])

    # ── 5. sh_run — multi-line output (test squasher) ──
    resp = server.request("tools/call", {
        "name": "sh_run",
        "arguments": {"cmd": "seq 1 50", "timeout": 10}
    }, req_id=11)

    text = extract_tool_text(resp)
    if "exit:0" in text:
        ok("seq 1 50", "exit:0")
    else:
        fail("seq 1 50", text[:200])

    # ── 6. sh_run — nonzero exit ──
    resp = server.request("tools/call", {
        "name": "sh_run",
        "arguments": {"cmd": "false", "timeout": 5}
    }, req_id=12)

    text = extract_tool_text(resp)
    # exit code should be non-zero (typically exit:1)
    if "exit:0" not in text and "exit:" in text:
        ok("nonzero exit", "non-zero exit code")
    else:
        fail("nonzero exit", text[:200])

    # ── 7. sh_run — denied command ──
    resp = server.request("tools/call", {
        "name": "sh_run",
        "arguments": {"cmd": "rm -rf /", "timeout": 5}
    }, req_id=13)

    if resp.get("error", {}).get("code") == -32005:
        ok("deny list", "rm -rf / blocked with -32005")
    else:
        fail("deny list", f"expected -32005, got {resp}")

    # ── 8. sh_session ──
    section("5. sh_session")

    # List (should have "main")
    resp = server.request("tools/call", {
        "name": "sh_session",
        "arguments": {"action": "list"}
    }, req_id=20)

    text = extract_tool_text(resp)
    if "main" in text:
        ok("list", "contains 'main' session")
    else:
        fail("list", text[:200])

    # Create
    resp = server.request("tools/call", {
        "name": "sh_session",
        "arguments": {"action": "create", "name": "smoke-sess", "shell": "/bin/bash"}
    }, req_id=21)

    text = extract_tool_text(resp)
    if "smoke-sess" in text:
        ok("create", "smoke-sess created")
    else:
        fail("create", text[:200])

    # Run on custom session
    resp = server.request("tools/call", {
        "name": "sh_run",
        "arguments": {"cmd": "echo on_custom_session", "session": "smoke-sess", "timeout": 5}
    }, req_id=22)

    text = extract_tool_text(resp)
    if "on_custom_session" in text:
        ok("run on custom session")
    else:
        fail("run on custom session", text[:200])

    # Close
    resp = server.request("tools/call", {
        "name": "sh_session",
        "arguments": {"action": "close", "name": "smoke-sess"}
    }, req_id=23)

    text = extract_tool_text(resp)
    if "smoke-sess" in text and "close" in text.lower():
        ok("close", "smoke-sess closed")
    else:
        fail("close", text[:200])

    # ── 9. sh_spawn + sh_interact ──
    section("6. sh_spawn + sh_interact")

    resp = server.request("tools/call", {
        "name": "sh_spawn",
        "arguments": {"alias": "bg1", "cmd": "sleep 30", "timeout": 5}
    }, req_id=30)

    text = extract_tool_text(resp)
    if "bg1" in text and "pid:" in text:
        ok("spawn", "bg1 spawned with pid")
    else:
        fail("spawn", text[:200])

    # Status
    resp = server.request("tools/call", {
        "name": "sh_interact",
        "arguments": {"alias": "bg1", "action": "status"}
    }, req_id=31)

    text = extract_tool_text(resp)
    if "bg1" in text and "status" in text:
        ok("status", "bg1 status ok")
    else:
        fail("status", text[:200])

    # Check digest includes bg1
    if "[procs]" in text and "bg1" in text:
        ok("digest includes bg1")
    else:
        fail("digest includes bg1", f"no [procs] with bg1 in: {text[:200]}")

    # Kill
    resp = server.request("tools/call", {
        "name": "sh_interact",
        "arguments": {"alias": "bg1", "action": "kill"}
    }, req_id=32)

    text = extract_tool_text(resp)
    if "bg1" in text and "kill" in text:
        ok("kill", "bg1 killed")
    else:
        fail("kill", text[:200])

    # ── 10. Error handling ──
    section("7. Error Handling")

    # Unknown tool
    resp = server.request("tools/call", {
        "name": "bogus_tool", "arguments": {}
    }, req_id=40)
    if resp.get("error", {}).get("code") == -32601:
        ok("unknown tool", "-32601")
    else:
        fail("unknown tool", str(resp))

    # Missing params
    resp = server.request("tools/call", {
        "name": "sh_run", "arguments": {}
    }, req_id=41)
    if resp.get("error", {}).get("code") == -32602:
        ok("missing params", "-32602")
    else:
        fail("missing params", str(resp))

    # Nonexistent session
    resp = server.request("tools/call", {
        "name": "sh_session",
        "arguments": {"action": "close", "name": "ghost"}
    }, req_id=42)
    if resp.get("error", {}).get("code") == -32002:
        ok("session not found", "-32002")
    else:
        fail("session not found", str(resp))

    # Nonexistent alias
    resp = server.request("tools/call", {
        "name": "sh_interact",
        "arguments": {"alias": "ghost", "action": "status"}
    }, req_id=43)
    if resp.get("error", {}).get("code") == -32003:
        ok("alias not found", "-32003")
    else:
        fail("alias not found", str(resp))

    # ── 11. Graceful shutdown ──
    section("8. Shutdown")

    exit_code = server.shutdown()
    if exit_code == 0:
        ok("graceful shutdown", f"exit={exit_code}")
    else:
        fail("graceful shutdown", f"exit={exit_code}")

    # ── Summary ──
    print(f"\n{'=' * 40}")
    total = passed + failed
    if failed == 0:
        print(f"{GREEN}{BOLD}{passed}/{total} passed{RESET}")
    else:
        print(f"{RED}{BOLD}{failed}/{total} failed{RESET}")
        for e in errors:
            print(f"  {RED}!{RESET} {e}")

    sys.exit(0 if failed == 0 else 1)


if __name__ == "__main__":
    main()
