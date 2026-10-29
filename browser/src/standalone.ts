import { app } from "electron";

import { servePages } from "./pages/scheme";
import { createSession } from "./session/session";
import type { SessionHandle } from "./session/session";

export async function runStandalone(cdpPort: number | null, argv: string[]): Promise<void> {
  let session: SessionHandle | undefined;
  let stopping = false;
  let finished = false;
  const finish = (code: number) => {
    if (finished) return;
    finished = true;
    for (const [signal, handler] of signals) process.removeListener(signal, handler);
    app.removeListener("before-quit", beforeQuit);
    if (!stopping) {
      stopping = true;
      session?.close(code);
    }
    app.exit(code);
  };
  const stop = (code: number) => {
    if (stopping || finished) return;
    stopping = true;
    if (session) session.close(code);
    else finish(code);
  };
  const beforeQuit = () => stop(0);
  const signals = [
    ["SIGINT", () => stop(130)],
    ["SIGTERM", () => stop(143)],
    ["SIGHUP", () => stop(129)],
  ] as const;
  for (const [signal, handler] of signals) process.on(signal, handler);
  app.on("before-quit", beforeQuit);
  try {
    servePages(() => session?.pageContext() ?? { cwd: process.cwd(), theme: null });
    session = createSession({
      key: `${process.pid}-standalone`,
      tty: process.env.PIXEL_TTY,
      argv,
      env: process.env,
      cwd: process.cwd(),
      cdpPort,
      onClose: finish,
    });
    await session.ready;
  } catch (error) {
    stop(1);
    throw error;
  }
}
