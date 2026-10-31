const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');
const { after, test } = require('node:test');

const state = { devices: [], checks: 0, discoveries: 0 };
const load = Module._load;
Module._load = function(request, parent, ...rest) {
  if (parent?.filename.endsWith('/dist/backend-auto.js')) {
    if (request === 'node:fs') return { ...fs, fstatSync(fd) { if (!state.devices[fd]) throw new Error('no fd'); return state.devices[fd]; } };
    if (request === './react/native') return { drmSession() {
      state.checks++;
      if (state.error) throw state.error;
      return { seat: 'seat0' };
    } };
  }
  return load.call(this, request, parent, ...rest);
};
const automatic = require('../dist/backend-auto.js');
const { resolveBackend } = require('../dist/backend.js');
const { discoverDrmDevices } = automatic;
after(() => { Module._load = load; });

function terminal(device, character = true) { return { rdev: device, isCharacterDevice: () => character }; }
function reset() { state.devices = [terminal(1027), terminal(1027)]; state.error = null; state.checks = 0; state.discoveries = 0; }

test('only matching physical VT descriptors qualify for automatic DRM, never environment hints or redirected streams', () => {
  reset();
  assert.equal(automatic.isLocalConsole({}, 'linux'), true);
  for (const platform of ['darwin', 'win32']) assert.equal(automatic.isLocalConsole({}, platform), false);
  for (const key of ['DISPLAY', 'WAYLAND_DISPLAY', 'SSH_CONNECTION', 'SSH_CLIENT', 'SSH_TTY', 'TMUX', 'STY', 'ZELLIJ', 'PIXEL_TTY', 'PIXEL_EMBED']) {
    assert.equal(automatic.isLocalConsole({ [key]: 'set' }, 'linux'), false, key);
  }
  assert.equal(automatic.isLocalConsole({ XDG_SESSION_TYPE: 'wayland' }, 'linux'), false);
  for (const devices of [[], [terminal(34816), terminal(34816)], [terminal(1280), terminal(1280)], [terminal(1024), terminal(1024)], [terminal(1088), terminal(1088)], [terminal(1027), terminal(1028)], [terminal(1027), terminal(1027, false)]]) {
    state.devices = devices;
    assert.equal(automatic.isLocalConsole({ TERM: 'linux', XDG_SESSION_TYPE: 'tty' }, 'linux'), false);
  }
});

test('automatic selection checks the real session before discovery, preserves explicit selection and never falls back after a session error', () => {
  reset();
  automatic.discoverDrmDevices = (options, seat) => {
    state.discoveries++;
    assert.equal(state.checks, 1);
    assert.equal(seat, 'seat0');
    return { ...options, card: '/dev/dri/card1', inputDevices: ['/dev/input/event2', '/dev/input/event5'], connector: 'eDP-1' };
  };
  try {
    assert.deepEqual(resolveBackend({}, {}, 'linux'), { name: 'drm', card: '/dev/dri/card1', inputDevices: ['/dev/input/event2', '/dev/input/event5'], connector: 'eDP-1', scaleFactor: 1 });
    assert.equal(state.discoveries, 1);
    reset();
    assert.deepEqual(resolveBackend({ backend: 'kitty' }, {}, 'linux'), { name: 'kitty' });
    assert.equal(state.checks, 0);
    state.error = new Error('inactive or remote session');
    assert.throws(() => resolveBackend({}, {}, 'linux'), /inactive or remote/);
    assert.equal(state.discoveries, 0);
    reset();
    state.devices = [terminal(34816), terminal(34816)];
    assert.deepEqual(resolveBackend({}, {}, 'linux'), { name: 'kitty' });
    assert.equal(state.checks, 0);
  } finally { automatic.discoverDrmDevices = discoverDrmDevices; }
});

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'pixel-auto-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const paths = { drm: path.join(root, 'sys/drm'), input: path.join(root, 'sys/input'), udev: path.join(root, 'udev'), dev: path.join(root, 'dev') };
  for (const directory of [paths.drm, paths.input, paths.udev, path.join(paths.dev, 'dri'), path.join(paths.dev, 'input')]) fs.mkdirSync(directory, { recursive: true });
  const device = (kind, name, number, props) => {
    const directory = path.join(paths[kind], name);
    fs.mkdirSync(directory, { recursive: true });
    fs.writeFileSync(path.join(directory, 'dev'), number);
    fs.writeFileSync(path.join(paths.udev, `c${number}`), Object.entries(props).map(([key, value]) => `E:${key}=${value}`).join('\n') + '\nV:1');
    fs.writeFileSync(path.join(paths.dev, kind === 'drm' ? 'dri' : 'input', name), '');
  };
  const output = (card, name) => {
    fs.mkdirSync(path.join(paths.drm, `${card}-${name}`), { recursive: true });
    fs.writeFileSync(path.join(paths.drm, `${card}-${name}`, 'status'), 'connected\n');
  };
  device('drm', 'card0', '226:0', {});
  device('drm', 'card1', '226:1', {});
  device('drm', 'card3', '226:3', { ID_SEAT: 'seat1' });
  output('card1', 'eDP-1');
  output('card3', 'HDMI-A-1');
  device('input', 'event2', '13:66', { ID_INPUT_KEYBOARD: '1', LIBINPUT_DEVICE_GROUP: 'keyboard' });
  device('input', 'event4', '13:68', { ID_INPUT_MOUSE: '1', LIBINPUT_DEVICE_GROUP: 'touchpad' });
  device('input', 'event5', '13:69', { ID_INPUT_TOUCHPAD: '1', LIBINPUT_DEVICE_GROUP: 'touchpad' });
  device('input', 'event6', '13:70', { ID_INPUT_KEYBOARD: '1', LIBINPUT_DEVICE_GROUP: 'touchpad' });
  device('input', 'event8', '13:72', { ID_INPUT_MOUSE: '1', LIBINPUT_DEVICE_GROUP: 'mouse' });
  device('input', 'event9', '13:73', { ID_INPUT_KEYBOARD: '1', ID_SEAT: 'seat1' });
  device('input', 'event10', '13:74', { ID_INPUT_KEYBOARD: '1', LIBINPUT_IGNORE_DEVICE: '1' });
  device('input', 'event11', '13:75', { ID_INPUT_KEY: '1' });
  return { paths, device, output };
}

test('discovery selects the seat output, dedicated keyboard and real touchpad, excluding companion, ignored and other-seat nodes', t => {
  const { paths } = fixture(t);
  assert.deepEqual(discoverDrmDevices({}, 'seat0', paths), {
    card: path.join(paths.dev, 'dri/card1'), connector: 'eDP-1',
    inputDevices: ['event2', 'event5', 'event8'].map(name => path.join(paths.dev, 'input', name)),
  });
  fs.renameSync(path.join(paths.input, 'event5'), path.join(paths.input, 'event15'));
  fs.renameSync(path.join(paths.dev, 'input/event5'), path.join(paths.dev, 'input/event15'));
  assert(discoverDrmDevices({}, 'seat0', paths).inputDevices.includes(path.join(paths.dev, 'input/event15')));
});

test('ambiguous displays and keyboards require overrides rather than an arbitrary device choice', t => {
  const { paths, device, output } = fixture(t);
  output('card1', 'HDMI-A-1');
  assert.throws(() => discoverDrmDevices({}, 'seat0', paths), /--drm-connector/);
  assert.equal(discoverDrmDevices({ connector: 'eDP-1' }, 'seat0', paths).connector, 'eDP-1');
  output('card0', 'DP-1');
  assert.throws(() => discoverDrmDevices({}, 'seat0', paths), /--drm-card/);
  const card = path.join(paths.dev, 'dri/card1');
  device('input', 'event12', '13:76', { ID_INPUT_KEYBOARD: '1', LIBINPUT_DEVICE_GROUP: 'second-keyboard' });
  assert.throws(() => discoverDrmDevices({ card }, 'seat0', paths), /--drm-input/);
  const options = { card, inputDevices: ['/dev/input/by-path/explicit-keyboard'], connector: 'eDP-1' };
  assert.deepEqual(discoverDrmDevices(options, 'seat0', paths), options);
});

test('missing metadata or device nodes do not become auto-selected devices', t => {
  const { paths } = fixture(t);
  fs.rmSync(path.join(paths.udev, 'c226:1'));
  assert.throws(() => discoverDrmDevices({}, 'seat0', paths), /candidates: none/);
  fs.rmSync(path.join(paths.dev, 'input/event2'));
  fs.rmSync(path.join(paths.dev, 'input/event6'));
  assert.throws(() => discoverDrmDevices({ card: '/dev/dri/card1' }, 'seat0', paths), /--drm-input/);
});
