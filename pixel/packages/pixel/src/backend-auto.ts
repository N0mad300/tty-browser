import fs from "node:fs";
import path from "node:path";

import type { DrmOptions } from "./backend";

export function isLocalConsole(env: NodeJS.ProcessEnv, platform: NodeJS.Platform): boolean {
  if (platform !== "linux") return false;
  if (["DISPLAY", "WAYLAND_DISPLAY", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "TMUX", "STY", "ZELLIJ", "PIXEL_TTY", "PIXEL_EMBED"].some((key) => env[key])) return false;
  if (env.XDG_SESSION_TYPE && env.XDG_SESSION_TYPE !== "tty") return false;
  try {
    const input = fs.fstatSync(0);
    const output = fs.fstatSync(1);
    const major = Math.floor(input.rdev / 256);
    const minor = input.rdev % 256;
    return input.isCharacterDevice() && output.isCharacterDevice() && input.rdev === output.rdev && major === 4 && minor > 0 && minor < 64;
  } catch {
    return false;
  }
}

export function localDrmSession(): { seat: string } {
  const { drmSession } = require("./react/native") as typeof import("./react/native");
  return drmSession();
}

interface DevicePaths {
  drm: string;
  input: string;
  udev: string;
  dev: string;
}

const systemPaths: DevicePaths = { drm: "/sys/class/drm", input: "/sys/class/input", udev: "/run/udev/data", dev: "/dev" };

function read(file: string): string {
  try { return fs.readFileSync(file, "utf8").trim(); } catch { return ""; }
}

function entries(directory: string): string[] {
  try { return fs.readdirSync(directory).sort(); } catch { return []; }
}

function properties(directory: string, name: string, paths: DevicePaths): Record<string, string> | null {
  const device = read(path.join(directory, name, "dev"));
  if (!/^\d+:\d+$/.test(device)) return null;
  const data = read(path.join(paths.udev, `c${device}`));
  if (!data) return null;
  return Object.fromEntries(data.split("\n").filter((line) => line.startsWith("E:")).map((line) => {
    const equal = line.indexOf("=");
    return [line.slice(2, equal), line.slice(equal + 1)];
  }));
}

export function discoverDrmDevices(options: DrmOptions, seat: string, paths: DevicePaths = systemPaths): DrmOptions {
  let { card, connector, inputDevices } = options;
  if (card === undefined) {
    const outputs = entries(paths.drm);
    const cards = outputs.filter((name) => /^card\d+$/.test(name)).flatMap((name) => {
      const info = properties(paths.drm, name, paths);
      if (!info || (info.ID_SEAT ?? "seat0") !== seat || !fs.existsSync(path.join(paths.dev, "dri", name))) return [];
      const connected = outputs.filter((output) => output.startsWith(`${name}-`) && read(path.join(paths.drm, output, "status")) === "connected").map((output) => output.slice(name.length + 1));
      return connected.length && (!connector || connected.includes(connector)) ? [{ name, connected }] : [];
    });
    if (cards.length !== 1) throw new Error(`cannot automatically select one DRM card on ${seat}; use --drm-card (candidates: ${cards.map((card) => card.name).join(", ") || "none"})`);
    card = path.join(paths.dev, "dri", cards[0].name);
    if (!connector) {
      if (cards[0].connected.length !== 1) throw new Error(`multiple connected outputs; use --drm-connector (${cards[0].connected.join(", ")})`);
      connector = cards[0].connected[0];
    }
  }
  if (inputDevices === undefined) {
    const devices = entries(paths.input).filter((name) => /^event\d+$/.test(name)).flatMap((name) => {
      const info = properties(paths.input, name, paths);
      const device = path.join(paths.dev, "input", name);
      return info && (info.ID_SEAT ?? "seat0") === seat && info.LIBINPUT_IGNORE_DEVICE !== "1" && fs.existsSync(device) ? [{ device, info }] : [];
    });
    const touchpadGroups = new Set(devices.filter(({ info }) => info.ID_INPUT_TOUCHPAD === "1").map(({ info }) => info.LIBINPUT_DEVICE_GROUP).filter(Boolean));
    const keyboards = devices.filter(({ info }) => info.ID_INPUT_KEYBOARD === "1");
    const dedicated = keyboards.filter(({ info }) => !touchpadGroups.has(info.LIBINPUT_DEVICE_GROUP));
    const candidates = dedicated.length ? dedicated : keyboards;
    if (candidates.length !== 1) throw new Error(`cannot automatically select one keyboard on ${seat}; use --drm-input for the keyboard and pointer (candidates: ${candidates.map(({ device }) => device).join(", ") || "none"})`);
    const pointers = devices.filter(({ info }) => info.ID_INPUT_TOUCHPAD === "1" || (info.ID_INPUT_MOUSE === "1" && !touchpadGroups.has(info.LIBINPUT_DEVICE_GROUP)));
    inputDevices = [...new Set([candidates[0].device, ...pointers.map(({ device }) => device)])];
  }
  return { ...options, card, connector, inputDevices };
}
