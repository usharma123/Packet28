#!/usr/bin/env node
// Unified entry point for the Packet28 CLI.
// Resolves the correct platform-specific binary and spawns it.

import { execSync, spawn } from "node:child_process";
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
  switch (platform) {
    case "darwin":
      if (arch === "arm64") return "darwin-arm64";
      if (arch === "x64") return "darwin-x64";
      break;
    case "linux":
      if (arch === "x64") return "linux-x64";
      if (arch === "arm64") return "linux-arm64";
      break;
  }
  return null;
}

function findBinary(name) {
  const platformKey = getPlatformKey();
  if (!platformKey) {
    throw new Error(
      `Unsupported platform: ${process.platform} (${process.arch}). ` +
        `Packet28 supports: darwin-arm64, darwin-x64, linux-x64, linux-arm64.`,
    );
  }

  const platformPackage = PLATFORM_PACKAGES[platformKey];

  // Try 1: resolve from the platform-specific optional dependency
  try {
    const pkgJsonPath = require.resolve(`${platformPackage}/package.json`);
    const vendorDir = path.join(path.dirname(pkgJsonPath), "bin");
    const binaryPath = path.join(vendorDir, name);
    if (existsSync(binaryPath)) return binaryPath;
  } catch {
    // optional dep not installed — fall through
  }

  // Try 2: local vendor directory (for development / cargo-built binaries)
  const localBinary = path.join(
    __dirname,
    "..",
    "vendor",
    platformKey,
    name,
  );
  if (existsSync(localBinary)) return localBinary;

  // Try 3: check if the binary is already on PATH
  try {
    const which = execSync(`which ${name}`, { encoding: "utf-8" }).trim();
    if (which && existsSync(which)) return which;
  } catch {
    // not on PATH
  }

  throw new Error(
    `Could not find ${name} binary. Reinstall: npm install -g packet28@latest`,
  );
}

const binaryPath = findBinary("Packet28");

// npm strips execute permissions from tarballs — fix on first run
try {
  const mode = statSync(binaryPath).mode;
  if (!(mode & 0o111)) {
    chmodSync(binaryPath, mode | 0o755);
    // Also fix packet28d (daemon) in the same directory
    const daemonPath = path.join(path.dirname(binaryPath), "packet28d");
    if (existsSync(daemonPath)) {
      chmodSync(daemonPath, 0o755);
    }
  }
} catch {
  // ignore — will fail at spawn if truly broken
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env: { ...process.env, PACKET28_MANAGED_BY_NPM: "1" },
});

child.on("error", (err) => {
  console.error(`Failed to start Packet28: ${err.message}`);
  process.exit(1);
});

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {
  process.on(sig, () => {
    if (!child.killed) {
      try {
        child.kill(sig);
      } catch {
        /* ignore */
      }
    }
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
