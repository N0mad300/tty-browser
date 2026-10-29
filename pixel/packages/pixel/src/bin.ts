#!/usr/bin/env node
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { backendEnvironment, parseLaunchArguments } from "./backend";
import { entryArguments } from "./entry";
import { appExecutable } from "./app-executable";
import { announceGuest, findOwner, waitForOwners } from "./instances";
import { apparmorSetup, linuxSandboxError } from "./sandbox";
import { UNSUPPORTED_ENV, checkTerminal, detect } from "./terminal";

function fail(message: string): never {
  process.stderr.write(`pixel: ${message}\n`);
  process.exit(1);
}

function resolveEntry(given: string): string {
  const target = path.resolve(given);
  if (fs.existsSync(target) && fs.statSync(target).isDirectory()) {
    const manifest = path.join(target, "package.json");
    let main = "index.js";
    if (fs.existsSync(manifest)) {
      const parsed = JSON.parse(fs.readFileSync(manifest, "utf8")) as { main?: string };
      if (parsed.main) main = parsed.main;
    }
    const entry = path.join(target, main);
    if (!fs.existsSync(entry)) {
      fail(`${entry} does not exist. Build your app or point "main" in package.json at the built entry.`);
    }
    return entry;
  }
  if (!fs.existsSync(target)) fail(`no such file ${target}`);
  return target;
}

function appName(dir: string): string {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8")) as { name?: string };
    if (parsed.name) return parsed.name.replace(/^@/, "").replace("/", "-");
  } catch {}
  return path.basename(dir);
}

function appDir(entry: string): string {
  for (let dir = path.dirname(entry); ; dir = path.dirname(dir)) {
    if (fs.existsSync(path.join(dir, "package.json"))) return dir;
    if (path.dirname(dir) === dir) return path.dirname(entry);
  }
}

function electronBinary(): string {
  const override = process.env.PIXEL_ELECTRON_APP;
  if (override) {
    try {
      return appExecutable(override);
    } catch (error) {
      fail((error as Error).message);
    }
  }
  const dist = path.resolve(__dirname, "..", "electron", "dist");
  if (!fs.existsSync(path.join(dist, ".zenbu-electron-sha256"))) {
    fail(
      "the patched electron build is not installed. Run `node node_modules/@zenbu-labs/pixel/scripts/postinstall.mjs` (npm normally runs it for you on install).",
    );
  }
  return process.platform === "darwin"
    ? path.join(dist, "Electron.app", "Contents", "MacOS", "pixel")
    : path.join(dist, "pixel");
}

function logFile(appDir: string): string {
  const state = process.env.XDG_STATE_HOME ?? path.join(os.homedir(), ".local", "state");
  const dir = path.join(state, "pixel", "logs");
  fs.mkdirSync(dir, { recursive: true });
  return path.join(dir, `${path.basename(appDir)}.stderr.log`);
}

function ownTty(): string {
  try {
    const out = execFileSync("tty", { stdio: ["inherit", "pipe", "ignore"], encoding: "utf8" }).trim();
    if (out.startsWith("/dev/")) return out;
  } catch {}
  return fail("could not work out which tty this shell is on");
}

async function main(): Promise<number> {
  const launch = parseLaunchArguments(process.argv.slice(2));
  if (launch.help) {
    process.stdout.write(
      "usage: pixel [entry|dir] [--backend auto|kitty|drm] [-- app args]\n" +
      "  DRM: --drm-card PATH --drm-input PATH [--drm-input PATH] [--drm-connector NAME] [--drm-scale NUMBER]\n" +
      "  DRM requires an active local Linux VT and a native build with DRM enabled.\n",
    );
    return 0;
  }
  const { backend, appArgs } = launch;
  const entry = resolveEntry(launch.entry);
  if (backend.name === "drm") {
    const { checkNativeBackend } = require("./react/native") as typeof import("./react/native");
    checkNativeBackend(backend);
  }
  let tty: string | undefined;
  let announced: ReturnType<typeof announceGuest> | null = null;
  let unsupported = false;
  if (backend.name === "kitty") {
    tty = process.env.PIXEL_TTY ?? ownTty();
    const owner = findOwner(tty);
    announced = owner ? announceGuest(owner, appName(appDir(entry))) : null;
    if (!owner && !process.env.PIXEL_EMBED) {
      if (!process.stdin.isTTY || !process.stdout.isTTY) fail("tty required");
      unsupported = (await checkTerminal(detect())).graphics === "unsupported";
    }
  }

  const electron = electronBinary();
  const chromiumArgs: string[] = [];
  if (process.platform === "linux") {
    let sandboxError = linuxSandboxError(electron);
    if (sandboxError && backend.name === "kitty") {
      apparmorSetup(electron);
      sandboxError = linuxSandboxError(electron);
    }
    if (sandboxError) fail(sandboxError);
    // headless ozone reports a 1x1 screen unless told otherwise:
    // https://source.chromium.org/chromium/chromium/src/+/refs/tags/150.0.7871.212:ui/ozone/platform/headless/headless_screen.cc;l=37-46
    if (!process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
      chromiumArgs.push("--ozone-platform=headless", "--screen-info={8192x8192}");
    }
  }

  const bootstrap = path.join(__dirname, "bootstrap.js");
  const env = backendEnvironment(backend, process.env);
  delete env.ELECTRON_RUN_AS_NODE;
  if (tty) env.PIXEL_TTY = tty;
  if (unsupported) env[UNSUPPORTED_ENV] = "1";
  else delete env[UNSUPPORTED_ENV];
  if (announced) env.PIXEL_PANE = announced.pane;
  else delete env.PIXEL_PANE;
  const log = logFile(appDir(entry));
  const stderr = fs.openSync(log, "a");
  const child = spawn(electron, entryArguments(entry, bootstrap, [...chromiumArgs, ...appArgs]), {
    stdio: ["inherit", "inherit", stderr],
    env,
  });
  const forward = (signal: NodeJS.Signals) => () => {
    try {
      child.kill(signal);
    } catch {}
    if (backend.name === "kitty") {
      setTimeout(() => {
        try { child.kill("SIGKILL"); } catch {}
      }, 2000).unref();
    }
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) process.on(signal, forward(signal));
  const exited = await new Promise<number>((resolve) => {
    child.on("error", (error) => {
      process.stderr.write(`pixel: could not start electron: ${error.message}\n`);
      resolve(1);
    });
    child.on("exit", (code, signal) => {
      if (code && !unsupported) {
        process.stderr.write(`pixel crashed (exit code ${code}). Full logs: ${log}\n`);
      }
      resolve(code ?? (signal ? 128 : 0));
    });
  });
  if (tty) await waitForOwners(tty);
  return exited;
}

void main().then(
  (code) => process.exit(code),
  (error: unknown) => fail(error instanceof Error ? error.message : String(error)),
);
