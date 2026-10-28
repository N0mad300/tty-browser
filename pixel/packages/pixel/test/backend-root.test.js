const assert = require("node:assert/strict");
const Module = require("node:module");
const React = require("react");
const { test } = require("node:test");

const state = { options: null, stopped: 0, scale: 0, exited: [], hooks: new Map() };
const forbidden = () => { throw new Error("DRM entered a terminal or desktop-screen path"); };
const engine = {
  info: { backend: "drm", width: 1001, height: 701, cellWidth: 0, cellHeight: 0, scaleFactor: 1.5, basePx: 24, kittyKeyboard: false, superKey: true, keyReleases: true, colors: {} },
  sharedTextures: false,
  stop() { state.stopped++; },
};
const mocks = {
  electron: { app: { isReady: () => true, getName: () => "test", on: (name, callback) => state.hooks.set(name, callback), exit: forbidden }, screen: new Proxy({}, { get: forbidden }) },
  "./react": { createRoot(options) { state.options = options; return engine; } },
  "./debug": { debug() {} },
  "./host/shell": { Shell: class {} },
  "./host/server": { OwnerServer: forbidden },
  "./host/strip": {},
  "./instances": { findOwner: forbidden, clearHandoff: forbidden, instancesDir: forbidden },
  "./registry": { RootContext: React.createContext(null), ViewRegistry: class {
    constructor(root, scale, noSuper) { state.scale = scale; state.noSuper = noSuper; }
    hosts() { return []; }
  } },
  "./webview": {},
  "./scale": { CellZoomFollower: class { ratio() { return null; } }, hostDisplayScale: forbidden },
  "./terminal": { detect: forbidden, UNSUPPORTED_ENV: "PIXEL_UNSUPPORTED_TERMINAL" },
  "./web/offscreen": { initOffscreenMode() {} },
  "./web/session": { installEmbedderApi() {}, uninstallEmbedderApi() {}, flushSessions() {} },
};
const load = Module._load;
Module._load = function (request, parent, ...rest) {
  if ((parent?.filename.endsWith("/dist/root.js") || request === "electron") && request in mocks) return mocks[request];
  return load.call(this, request, parent, ...rest);
};
const { createRoot } = require("../dist/root.js");
Module._load = load;

function root() {
  return createRoot({
    tty: "/dev/pts/old-terminal", wrapper: "tmux",
    backend: "drm", drm: { card: "/dev/dri/card1", inputDevices: ["/dev/input/event2"], scaleFactor: 1.5 },
    sessionEnv: { PIXEL_UNSUPPORTED_TERMINAL: "1", PIXEL_TTY: "/dev/pts/old-terminal", PIXEL_EMBED: "old-pane-host" },
    onExit: (code) => state.exited.push(code),
  });
}

test("a DRM root skips terminal probes, owners, wrappers and virtual-screen DPI", { skip: process.platform !== "linux" }, () => {
  const created = root();
  assert.equal(state.options.backend, "drm");
  assert.equal(state.options.tty, undefined);
  assert.equal(state.options.host, undefined);
  assert.equal(state.options.wrapper, undefined);
  assert.equal(state.scale, 1.5);
  assert.equal(state.noSuper, false);
  assert.equal(created.hosted, false);
  assert.equal(created.info.width, 1001);
  created.stop();
  assert.equal(state.stopped, 1);
  assert.deepEqual(state.exited, [0]);
});

test("application quit completes engine cleanup once without calling app.exit recursively", { skip: process.platform !== "linux" }, () => {
  const created = root();
  state.hooks.get("before-quit")();
  assert.equal(state.stopped, 2);
  created.stop();
  assert.equal(state.stopped, 2);
  assert.deepEqual(state.exited, [0]);
});
