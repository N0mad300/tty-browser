const assert = require("node:assert/strict");
const { test } = require("node:test");
const { backendEnvironment, parseLaunchArguments, resolveBackend } = require("../dist/backend.js");

const env = {
  PIXEL_BACKEND: "drm",
  PIXEL_DRM_CARD: "/dev/dri/card1",
  PIXEL_DRM_INPUTS: '["/dev/input/event2","/dev/input/event4"]',
  PIXEL_DRM_SCALE: "1.5",
};

test("desktop environments and a TTY environment variable alone do not select DRM", () => {
  assert.deepEqual(resolveBackend({}, { WAYLAND_DISPLAY: "wayland-1" }, "linux"), { name: "kitty" });
  assert.deepEqual(resolveBackend({}, { XDG_SESSION_TYPE: "tty" }, "linux"), { name: "kitty" });
  assert.deepEqual(resolveBackend({ backend: "kitty" }, env), { name: "kitty" });
});

test("explicit DRM configuration reaches the native boundary with exact scale and devices", () => {
  assert.deepEqual(resolveBackend({}, env, "linux"), {
    name: "drm", card: "/dev/dri/card1", inputDevices: ["/dev/input/event2", "/dev/input/event4"], scaleFactor: 1.5, connector: undefined,
  });
  const options = { backend: "drm", drm: { card: "/dev/dri/card0", inputDevices: ["/dev/input/by-id/keyboard"], connector: "HDMI-A-1", scaleFactor: 2 } };
  assert.deepEqual(resolveBackend(options, env, "linux"), { name: "drm", ...options.drm });
});

test("invalid or unsupported choices fail before terminal probing or device acquisition", () => {
  assert.throws(() => resolveBackend({ backend: "sixel" }), /unknown Pixel backend/);
  assert.throws(() => resolveBackend({}, env, "darwin"), /requires Linux/);
  assert.throws(() => resolveBackend({}, { ...env, PIXEL_DRM_CARD: "" }, "linux"), /requires a card/);
  for (const value of ['"/dev/input/event2"', '[1]', '[]', 'broken']) {
    assert.throws(() => resolveBackend({}, { ...env, PIXEL_DRM_INPUTS: value }, "linux"), /input device paths|JSON array/);
  }
  for (const value of ["NaN", "Infinity", "0", "-1", "5"]) {
    assert.throws(() => resolveBackend({}, { ...env, PIXEL_DRM_SCALE: value }, "linux"), /DRM scale/);
  }
  assert.throws(() => resolveBackend({ backend: "auto", drm: { card: "x", inputDevices: ["y"] } }), /require backend/);
});

test("a DRM child inherits device configuration without Kitty ownership and probe flags", () => {
  const source = { ...env, PIXEL_TTY: "/dev/pts/3", PIXEL_EMBED: "host", PIXEL_PANE: "pane", PIXEL_UNSUPPORTED_TERMINAL: "1" };
  const child = backendEnvironment(resolveBackend({}, env, "linux"), source);
  for (const key of ["PIXEL_TTY", "PIXEL_EMBED", "PIXEL_PANE", "PIXEL_UNSUPPORTED_TERMINAL"]) assert.equal(child[key], undefined);
  assert.equal(child.PIXEL_BACKEND, "drm");
  assert.equal(child.LIBSEAT_BACKEND, "logind");
  assert.equal(child.PIXEL_DRM_SCALE, "1.5");
  assert.equal(source.PIXEL_TTY, "/dev/pts/3");
});

test("launcher keeps application arguments separate from backend options", () => {
  const parsed = parseLaunchArguments(["--backend=drm", "--drm-card", "/dev/dri/card1", "--drm-input", "/dev/input/event2", "--drm-input=/dev/input/event4", "--drm-scale", "2", "app", "--", "https://example.com", "--backend", "application-option"], {}, "linux");
  assert.equal(parsed.entry, "app");
  assert.equal(parsed.backend.name, "drm");
  assert.equal(parsed.backend.scaleFactor, 2);
  assert.deepEqual(parsed.backend.inputDevices, ["/dev/input/event2", "/dev/input/event4"]);
  assert.deepEqual(parsed.appArgs, ["https://example.com", "--backend", "application-option"]);
  assert.equal(parseLaunchArguments(["--help"]).help, true);
  assert.throws(() => parseLaunchArguments(["--backend"]), /requires a value/);
  assert.throws(() => parseLaunchArguments(["--unknown"]), /unknown option/);
  assert.throws(() => parseLaunchArguments(["app", "url"]), /after --/);
});
