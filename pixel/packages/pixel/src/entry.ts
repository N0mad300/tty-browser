import fs from "node:fs";
import path from "node:path";

export function entryArguments(entry: string, bootstrap: string, args: string[]): string[] {
  for (let directory = path.dirname(entry); ; directory = path.dirname(directory)) {
    const manifest = path.join(directory, "package.json");
    if (fs.existsSync(manifest)) {
      const config = JSON.parse(fs.readFileSync(manifest, "utf8")) as { pixel?: { bootstrap?: unknown } };
      const enabled = config.pixel?.bootstrap;
      if (enabled !== undefined && typeof enabled !== "boolean") {
        throw new Error(`${manifest}: pixel.bootstrap must be a boolean`);
      }
      return enabled === false ? [entry, ...args] : [bootstrap, entry, ...args];
    }
    if (path.dirname(directory) === directory) return [bootstrap, entry, ...args];
  }
}
