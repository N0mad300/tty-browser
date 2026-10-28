export type BackendName = "auto" | "kitty" | "drm";

export interface DrmOptions {
  card: string;
  inputDevices: string[];
  connector?: string;
  scaleFactor?: number;
}

export interface BackendOptions {
  backend?: BackendName;
  drm?: DrmOptions;
}

export type ResolvedBackend =
  | { name: "kitty" }
  | { name: "drm"; card: string; inputDevices: string[]; connector?: string; scaleFactor: number };

export function resolveBackend(
  options: BackendOptions = {},
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
): ResolvedBackend {
  const name = options.backend ?? env.PIXEL_BACKEND ?? "auto";
  if (name === "auto" || name === "kitty") {
    if (options.drm) throw new Error("DRM options require backend: 'drm'");
    return { name: "kitty" };
  }
  if (name !== "drm") throw new Error(`unknown Pixel backend: ${name}`);
  if (platform !== "linux") throw new Error("the DRM backend requires Linux");
  const card = options.drm?.card ?? env.PIXEL_DRM_CARD;
  let inputDevices: unknown = options.drm?.inputDevices;
  if (inputDevices === undefined && env.PIXEL_DRM_INPUTS !== undefined) {
    try {
      inputDevices = JSON.parse(env.PIXEL_DRM_INPUTS);
    } catch {
      throw new Error("PIXEL_DRM_INPUTS must be a JSON array of input device paths");
    }
  }
  if (!card) throw new Error("the DRM backend requires a card: use --drm-card or PIXEL_DRM_CARD");
  if (!Array.isArray(inputDevices) || inputDevices.length === 0 || inputDevices.some((device) => typeof device !== "string" || !device)) {
    throw new Error("the DRM backend requires input device paths: use --drm-input or PIXEL_DRM_INPUTS");
  }
  const connector = options.drm?.connector ?? env.PIXEL_DRM_CONNECTOR;
  const scaleFactor = options.drm?.scaleFactor ?? Number(env.PIXEL_DRM_SCALE ?? 1);
  if (!Number.isFinite(scaleFactor) || scaleFactor <= 0 || scaleFactor > 4) {
    throw new Error("DRM scale must be greater than zero, up to 4");
  }
  return { name, card, inputDevices, connector, scaleFactor };
}

export function backendEnvironment(backend: ResolvedBackend, env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const next: NodeJS.ProcessEnv = { ...env, PIXEL_BACKEND: backend.name };
  if (backend.name === "drm") {
    next.LIBSEAT_BACKEND ??= "logind";
    next.PIXEL_DRM_CARD = backend.card;
    next.PIXEL_DRM_INPUTS = JSON.stringify(backend.inputDevices);
    next.PIXEL_DRM_SCALE = String(backend.scaleFactor);
    if (backend.connector) next.PIXEL_DRM_CONNECTOR = backend.connector;
    else delete next.PIXEL_DRM_CONNECTOR;
    for (const key of ["PIXEL_TTY", "PIXEL_EMBED", "PIXEL_PANE", "PIXEL_UNSUPPORTED_TERMINAL"]) delete next[key];
  }
  return next;
}

export function parseLaunchArguments(args: string[], env: NodeJS.ProcessEnv = process.env, platform: NodeJS.Platform = process.platform): {
  entry: string;
  appArgs: string[];
  backend: ResolvedBackend;
  help: boolean;
} {
  const separator = args.indexOf("--");
  const own = separator < 0 ? args : args.slice(0, separator);
  const appArgs = separator < 0 ? [] : args.slice(separator + 1);
  if (own.includes("--help") || own.includes("-h")) {
    return { entry: ".", appArgs, backend: { name: "kitty" }, help: true };
  }
  const options: BackendOptions = {};
  const drm: Partial<DrmOptions> = {};
  let entry: string | undefined;
  for (let at = 0; at < own.length; at++) {
    const argument = own[at];
    if (!argument.startsWith("-")) {
      if (entry !== undefined) throw new Error(`unexpected ${argument}; put application arguments after --`);
      entry = argument;
      continue;
    }
    const equal = argument.indexOf("=");
    const flag = equal < 0 ? argument : argument.slice(0, equal);
    if (!["--backend", "--drm-card", "--drm-input", "--drm-connector", "--drm-scale"].includes(flag)) {
      throw new Error(`unknown option ${flag}; put application arguments after --`);
    }
    const value = equal < 0 ? own[++at] : argument.slice(equal + 1);
    if (!value || value.startsWith("--")) throw new Error(`${flag} requires a value`);
    switch (flag) {
      case "--backend": options.backend = value as BackendName; break;
      case "--drm-card": drm.card = value; break;
      case "--drm-input": (drm.inputDevices ??= []).push(value); break;
      case "--drm-connector": drm.connector = value; break;
      case "--drm-scale": drm.scaleFactor = Number(value); break;
    }
  }
  if (Object.keys(drm).length) {
    options.drm = {
      card: drm.card ?? env.PIXEL_DRM_CARD ?? "",
      inputDevices: drm.inputDevices ?? resolveInputs(env.PIXEL_DRM_INPUTS),
      connector: drm.connector,
      scaleFactor: drm.scaleFactor,
    };
  }
  return { entry: entry ?? ".", appArgs, backend: resolveBackend(options, env, platform), help: false };
}

function resolveInputs(value: string | undefined): string[] {
  if (value === undefined) return [];
  try { return JSON.parse(value); }
  catch { throw new Error("PIXEL_DRM_INPUTS must be a JSON array of input device paths"); }
}
