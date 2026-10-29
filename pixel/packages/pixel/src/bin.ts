#!/usr/bin/env node
import { launch } from "./launcher";

void launch(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (error: unknown) => {
    process.stderr.write(`pixel: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  },
);
