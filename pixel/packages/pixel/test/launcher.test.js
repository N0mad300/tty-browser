const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');
const { after, test } = require('node:test');

const state = {};
const load = Module._load;
Module._load = function(request, parent, ...rest) {
  if (parent?.filename.endsWith('/dist/launcher.js')) {
    if (request === 'node:child_process') return {
      spawn(executable, args, options) {
        state.child = { executable, args, options };
        const child = new EventEmitter();
        child.kill = signal => { state.signal = signal; };
        process.nextTick(() => child.emit('exit', 0, null));
        return child;
      },
    };
    if (request === './terminal') return { detect() {}, checkTerminal() { throw new Error('terminal probe'); } };
    if (request === './instances') return { waitForOwners: async () => {} };
    if (request === './sandbox') return { linuxSandboxError: () => null };
    if (request === './react/native') return { checkNativeBackend(backend) { state.checked = backend; } };
  }
  return load.call(this, request, parent, ...rest);
};
const { launch } = require('../dist/launcher.js');
after(() => { Module._load = load; });

test('the public launcher forwards DRM through Pixel without terminal probes and releases its listeners and log fd', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'pixel-launch-'));
  const previous = { ...process.env };
  const listeners = ['SIGINT', 'SIGTERM', 'SIGHUP'].map(signal => process.listenerCount(signal));
  try {
    fs.writeFileSync(path.join(directory, 'main.js'), '');
    fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify({ main: 'main.js', pixel: { bootstrap: false } }));
    process.env.PIXEL_ELECTRON_APP = process.execPath;
    process.env.XDG_STATE_HOME = directory;
    process.env.PIXEL_TTY = '/dev/pts/123';
    assert.equal(await launch([directory, '--backend', 'drm', '--drm-card', '/dev/dri/card1', '--drm-input', '/dev/input/event2', '--', '--standalone', 'https://example.com']), 0);
    assert.equal(state.checked.name, 'drm');
    assert.equal(state.child.executable, process.execPath);
    assert.equal(state.child.args[0], path.join(directory, 'main.js'));
    assert.deepEqual(state.child.args.slice(-2), ['--standalone', 'https://example.com']);
    assert.equal(state.child.options.env.PIXEL_BACKEND, 'drm');
    assert.equal(state.child.options.env.PIXEL_TTY, undefined);
    assert.throws(() => fs.fstatSync(state.child.options.stdio[2]), /EBADF/);
    assert.deepEqual(['SIGINT', 'SIGTERM', 'SIGHUP'].map(signal => process.listenerCount(signal)), listeners);
  } finally {
    for (const key of Object.keys(process.env)) if (!(key in previous)) delete process.env[key];
    Object.assign(process.env, previous);
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('invalid launcher arguments reject without exiting the calling application', async () => {
  await assert.rejects(launch(['--backend', 'invalid']), /unknown Pixel backend/);
});
