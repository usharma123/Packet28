#!/usr/bin/env node
// Shortcut for `packet28 mcp serve` — designed for MCP server config.
// Usage in claude_desktop_config.json:
//   { "command": "packet28-mcp", "args": ["--root", "."] }

import { spawn } from "node:child_process";
import { chmodSync, existsSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const require = createRequire(import.meta.url);

const PLATFORM_PACKAGES = {
  "darwin-arm64": "@packet28/darwin-arm64",
  "darwin-x64": "@packet28/darwin-x64",
  "linux-x64": "@packet28/linux-x64",
  "linux-arm64": "@packet28/linux-arm64",
};

function getPlatformKey() {
  const { platform, arch } = process;
  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "darwin" && arch === "x64") return "darwin-x64";
  if (platform === "linux" && arch === "x64") return "linux-x64";
  if (platform === "linux" && arch === "arm64") return "linux-arm64";
  return null;
}

function findBinary(name) {
  const platformKey = getPlatformKey();
  if (!platformKey) {
    throw new Error(`Unsupported platform: ${process.platform} (${process.arch})`);
  }

  const platformPackage = PLATFORM_PACKAGES[platformKey];
  try {
    const pkgJsonPath = require.resolve(`${platformPackage}/package.json`);
    const bin = path.join(path.dirname(pkgJsonPath), "bin", name);
    if (existsSync(bin)) return bin;
  } catch { /* fall through */ }

  const local = path.join(__dirname, "..", "vendor", platformKey, name);
  if (existsSync(local)) return local;

  throw new Error(`Could not find ${name}. Reinstall: npm install -g packet28@latest`);
}

const binaryPath = findBinary("Packet28");

// npm strips execute permissions from tarballs — fix on first run
try {
  const mode = statSync(binaryPath).mode;
  if (!(mode & 0o111)) {
    chmodSync(binaryPath, mode | 0o755);
  }
} catch {
  // ignore
}

// Prepend "mcp serve" to the user's args
const child = spawn(binaryPath, ["mcp", "serve", ...process.argv.slice(2)], {
  stdio: "inherit",
  env: { ...process.env, PACKET28_MANAGED_BY_NPM: "1" },
});

child.on("error", (err) => {
  console.error(`Failed to start Packet28 MCP server: ${err.message}`);
  process.exit(1);
});

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {
  process.on(sig, () => {
    if (!child.killed) try { child.kill(sig); } catch { /* ignore */ }
  });
});

const result = await new Promise((resolve) => {
  child.on("exit", (code, signal) => {
    resolve(signal ? { type: "signal", signal } : { type: "code", code: code ?? 1 });
  });
});

if (result.type === "signal") {
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.code);
}
