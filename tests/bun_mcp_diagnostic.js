#!/usr/bin/env bun
//
// PTY Environment Diagnostic — Bun spawn path
//
// Reproduces Claude Code's exact MCP server spawn behavior:
// Bun.spawn() with stdio: ["pipe", "pipe", "pipe"], which uses unix
// socket pairs instead of POSIX pipes.
//
// Usage: MISH_BIN=/path/to/mish bun run tests/bun_mcp_diagnostic.js
//
// Outputs a single JSON line to stdout:
//   { pass: bool, output?: string, error?: string, elapsed_ms: number, stderr: string }

const MISH_BIN = process.env.MISH_BIN;
if (!MISH_BIN) {
  console.log(JSON.stringify({ pass: false, error: "MISH_BIN env var required", elapsed_ms: 0, stderr: "" }));
  process.exit(1);
}

const TIMEOUT_MS = 15_000;

async function main() {
  const started = Date.now();
  let stderrBuf = "";

  // Spawn exactly like Claude Code does: Bun.spawn with all three stdio piped.
  // Bun internally creates unix socket pairs (AF_UNIX, SOCK_STREAM) for these,
  // NOT POSIX pipes — this is the key difference from Rust's Command::spawn.
  const proc = Bun.spawn([MISH_BIN, "serve"], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, SHELL: "/bin/bash" },
  });

  // Drain stderr in background (non-blocking)
  (async () => {
    try {
      const reader = proc.stderr.getReader();
      const dec = new TextDecoder();
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        stderrBuf += dec.decode(value, { stream: true });
      }
    } catch {}
  })();

  // Single stdout reader — kept for the lifetime of the test
  const stdoutReader = proc.stdout.getReader();
  const dec = new TextDecoder();
  let stdoutBuf = "";

  async function readLine() {
    while (true) {
      const nl = stdoutBuf.indexOf("\n");
      if (nl >= 0) {
        const line = stdoutBuf.substring(0, nl);
        stdoutBuf = stdoutBuf.substring(nl + 1);
        return line;
      }
      const { done, value } = await stdoutReader.read();
      if (done) throw new Error("stdout closed before newline");
      stdoutBuf += dec.decode(value, { stream: true });
    }
  }

  async function send(obj) {
    proc.stdin.write(JSON.stringify(obj) + "\n");
    await proc.stdin.flush();
  }

  // Global timeout — kill and report if we hang
  const timer = setTimeout(() => {
    try { proc.kill(); } catch {}
    console.log(JSON.stringify({
      pass: false,
      error: `global timeout (${TIMEOUT_MS}ms)`,
      elapsed_ms: Date.now() - started,
      stderr: stderrBuf.substring(0, 4000),
    }));
    process.exit(1);
  }, TIMEOUT_MS);

  try {
    // 1. Initialize (camelCase — matching MCP types with #[serde(rename_all = "camelCase")])
    await send({
      jsonrpc: "2.0", id: 1, method: "initialize",
      params: {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "bun-diagnostic" },
      },
    });

    const initLine = await readLine();
    const initResp = JSON.parse(initLine);
    if (initResp?.result?.serverInfo?.name !== "mish") {
      throw new Error("bad init response: " + initLine.substring(0, 300));
    }

    // 2. Notification (no response expected)
    await send({ jsonrpc: "2.0", id: null, method: "notifications/initialized" });
    await Bun.sleep(50);

    // 3. sh_run echo diag_ok
    await send({
      jsonrpc: "2.0", id: 100, method: "tools/call",
      params: { name: "sh_run", arguments: { cmd: "echo diag_ok", timeout: 10 } },
    });

    const runLine = await readLine();
    const runResp = JSON.parse(runLine);

    if (runResp?.error) {
      throw new Error("sh_run error: " + JSON.stringify(runResp.error).substring(0, 500));
    }

    // Extract output — content-wrapped format (result.content[0].text → JSON)
    let output = "";
    const text = runResp?.result?.content?.[0]?.text;
    if (text) {
      try {
        const payload = JSON.parse(text);
        output = payload?.result?.output || "";
      } catch {
        output = text; // fallback: use raw text
      }
    }
    // Fallback: direct result access
    if (!output) {
      output = runResp?.result?.result?.output || "";
    }

    clearTimeout(timer);
    console.log(JSON.stringify({
      pass: output.includes("diag_ok"),
      output: output.substring(0, 1000),
      elapsed_ms: Date.now() - started,
      stderr: stderrBuf.substring(0, 4000),
    }));

  } catch (err) {
    clearTimeout(timer);
    console.log(JSON.stringify({
      pass: false,
      error: String(err?.message || err),
      elapsed_ms: Date.now() - started,
      stderr: stderrBuf.substring(0, 4000),
    }));
  } finally {
    try { proc.stdin.end(); } catch {}
    // Give the process a moment to exit, then force kill
    setTimeout(() => { try { proc.kill(); } catch {} }, 1000);
  }
}

main();
