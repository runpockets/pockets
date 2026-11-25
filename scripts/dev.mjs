#!/usr/bin/env node
// Starts the local Rust Pockets daemon and the Vite dev server in one terminal,
// opens the authenticated dev dashboard URL once both are ready, and tears down
// both process groups cleanly on Ctrl+C.

import { spawn } from "node:child_process";
import process from "node:process";

const args = process.argv.slice(2);
let port = "18786";
let dataDir = null;
let autoOpen = true;

for (let i = 0; i < args.length; i += 1) {
  const arg = args[i];
  if (arg === "--no-open") {
    autoOpen = false;
  } else if (arg === "--port" && args[i + 1]) {
    port = args[++i];
  } else if (arg.startsWith("--port=")) {
    port = arg.slice("--port=".length);
  } else if (arg === "--data-dir" && args[i + 1]) {
    dataDir = args[++i];
  } else if (arg.startsWith("--data-dir=")) {
    dataDir = arg.slice("--data-dir=".length);
  } else if (arg === "-h" || arg === "--help") {
    console.log(
      "Usage: npm run dev [-- --port PORT] [-- --data-dir DIR] [-- --no-open]",
    );
    process.exit(0);
  } else {
    console.error(`Unknown option: ${arg}`);
    process.exit(2);
  }
}

if (!/^\d+$/.test(port) || Number(port) < 1 || Number(port) > 65535) {
  console.error(`Invalid port: ${port}`);
  process.exit(2);
}

const isWin = process.platform === "win32";
const useColor = Boolean(process.stdout.isTTY);
const ansiPattern = /\x1b\[[0-9;]*m/g;

function tag(label, colorCode) {
  return useColor ? `\x1b[${colorCode}m[${label}]\x1b[0m` : `[${label}]`;
}

function pipePrefixed(stream, prefix, target, onLine) {
  let buffer = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    buffer += chunk;
    const lines = buffer.split(/\r?\n/);
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      target.write(`${prefix} ${line}\n`);
      onLine?.(line.replace(ansiPattern, ""));
    }
  });
  stream.on("end", () => {
    if (buffer.length > 0) {
      target.write(`${prefix} ${buffer}\n`);
      onLine?.(buffer.replace(ansiPattern, ""));
    }
  });
}

function openBrowser(url) {
  const [cmd, cmdArgs] =
    process.platform === "darwin"
      ? ["open", [url]]
      : process.platform === "win32"
        ? ["cmd", ["/c", "start", "", url]]
        : ["xdg-open", [url]];
  const opener = spawn(cmd, cmdArgs, { stdio: "ignore", detached: true });
  opener.on("error", () => {});
  opener.unref();
}

const daemonArgs = ["run", "-p", "pockets-cli", "--"];
if (dataDir) {
  daemonArgs.push("--data-dir", dataDir);
}
daemonArgs.push(
  "serve",
  "--port",
  port,
  "--dev-origin",
  "http://localhost:1420",
);

const daemon = spawn("cargo", daemonArgs, {
  stdio: ["ignore", "pipe", "pipe"],
  detached: !isWin,
});

const npmBin = isWin ? "npm.cmd" : "npm";
const web = spawn(npmBin, ["run", "dev:web"], {
  stdio: ["ignore", "pipe", "pipe"],
  detached: !isWin,
  env: {
    ...process.env,
    POCKETS_DAEMON_URL: `http://127.0.0.1:${port}`,
  },
});

const children = [daemon, web];
let shuttingDown = false;
let devUrl = null;
let viteReady = false;
let opened = false;

function maybeOpen() {
  if (!autoOpen || opened || !devUrl || !viteReady) return;
  opened = true;
  console.log(`${tag("dev", 32)} Opening http://localhost:1420/ in browser…`);
  openBrowser(devUrl);
}

function terminate(child, signal) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  try {
    if (!isWin && child.pid) {
      process.kill(-child.pid, signal);
    } else {
      child.kill(signal);
    }
  } catch {
    // Process already exited.
  }
}

function shutdown(exitCode) {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const child of children) {
    terminate(child, "SIGTERM");
  }
  const forceTimer = setTimeout(() => {
    for (const child of children) {
      terminate(child, "SIGKILL");
    }
    process.exit(exitCode);
  }, 3000);
  forceTimer.unref();

  let remaining = children.filter(
    (c) => c.exitCode === null && c.signalCode === null,
  ).length;
  if (remaining === 0) {
    clearTimeout(forceTimer);
    process.exit(exitCode);
  }
  for (const child of children) {
    if (child.exitCode !== null || child.signalCode !== null) continue;
    child.once("exit", () => {
      remaining -= 1;
      if (remaining <= 0) {
        clearTimeout(forceTimer);
        process.exit(exitCode);
      }
    });
  }
}

const daemonTag = tag("daemon", 36);
const webTag = tag("web", 35);

pipePrefixed(daemon.stdout, daemonTag, process.stdout, (line) => {
  const match = line.match(
    /^Dev dashboard URL:\s*(http:\/\/localhost:1420\/auth\/bootstrap\?token=\S+)/,
  );
  if (match) {
    devUrl = match[1];
    maybeOpen();
  }
});
pipePrefixed(daemon.stderr, daemonTag, process.stderr);

pipePrefixed(web.stdout, webTag, process.stdout, (line) => {
  if (line.includes("http://localhost:1420")) {
    viteReady = true;
    maybeOpen();
  }
});
pipePrefixed(web.stderr, webTag, process.stderr);

daemon.on("exit", (code) => {
  if (!shuttingDown) {
    console.error(
      `${tag("dev", 31)} Daemon exited (${code ?? "signal"}). If Homebrew Pockets is running on port ${port}, run \`brew services stop pockets\` or pass \`-- --port 19786\`.`,
    );
    shutdown(code || 1);
  }
});

web.on("exit", (code) => {
  if (!shuttingDown) {
    console.error(
      `${tag("dev", 31)} Vite dev server exited (${code ?? "signal"}).`,
    );
    shutdown(code || 1);
  }
});

process.on("SIGINT", () => shutdown(0));
process.on("SIGTERM", () => shutdown(0));
