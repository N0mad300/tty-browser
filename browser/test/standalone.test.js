const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const Module = require('node:module');
const { test } = require('node:test');

const app = new EventEmitter();
const state = {};
app.exit = code => state.events.push(['exit', code]);
const load = Module._load;
Module._load = function(request, parent, ...rest) {
  if (parent?.filename.endsWith('/dist/standalone.js')) {
    if (request === 'electron') return { app };
    if (request === './pages/scheme') return { servePages(context) { state.context = context; state.events.push(['protocols']); } };
    if (request === './session/session') return { createSession(options) {
      state.options = options;
      state.events.push(['session']);
      if (state.fail) throw new Error('session failed');
      return {
        ready: state.ready ?? Promise.resolve(),
        pageContext: () => ({ cwd: '/session', theme: null }),
        close(code) { state.events.push(['close', code]); options.onClose(code); },
      };
    } };
  }
  return load.call(this, request, parent, ...rest);
};
const { runStandalone } = require('../dist/standalone.js');
Module._load = load;

function reset() { state.events = []; state.fail = false; state.ready = null; }

test('foreground startup serves protocols, forwards session context, and exits after session cleanup', async () => {
  reset();
  const listeners = process.listenerCount('SIGTERM');
  await runStandalone(12345, ['https://example.com']);
  assert.deepEqual(state.events, [['protocols'], ['session']]);
  assert.equal(state.options.cdpPort, 12345);
  assert.equal(state.options.env, process.env);
  assert.equal(state.options.cwd, process.cwd());
  assert.deepEqual(state.options.argv, ['https://example.com']);
  assert.deepEqual(state.context(), { cwd: '/session', theme: null });
  app.emit('before-quit');
  assert.deepEqual(state.events.slice(-2), [['close', 0], ['exit', 0]]);
  state.options.onClose(1);
  assert.deepEqual(state.events.slice(-2), [['close', 0], ['exit', 0]]);
  assert.equal(process.listenerCount('SIGTERM'), listeners);
  assert.equal(app.listenerCount('before-quit'), 0);
});

test('termination restores the session before exiting and removes its signal handlers', async () => {
  reset();
  const before = new Set(process.listeners('SIGTERM'));
  await runStandalone(null, []);
  const handler = process.listeners('SIGTERM').find(listener => !before.has(listener));
  assert.ok(handler);
  handler();
  assert.deepEqual(state.events.slice(-2), [['close', 143], ['exit', 143]]);
  assert.equal(process.listeners('SIGTERM').some(listener => !before.has(listener)), false);
});

test('engine-originated exit disposes the browser session before exiting the application', async () => {
  reset();
  await runStandalone(null, []);
  state.options.onClose(0);
  assert.deepEqual(state.events.slice(-2), [['close', 0], ['exit', 0]]);
  assert.equal(app.listenerCount('before-quit'), 0);
});

test('startup failure exits cleanly and leaves no foreground lifecycle handlers', async () => {
  reset();
  state.fail = true;
  const listeners = process.listenerCount('SIGINT');
  await assert.rejects(runStandalone(null, []), /session failed/);
  assert.deepEqual(state.events.at(-1), ['exit', 1]);
  assert.equal(process.listenerCount('SIGINT'), listeners);
  assert.equal(app.listenerCount('before-quit'), 0);
});


test('an asynchronous startup failure closes the created session before exit', async () => {
  reset();
  state.ready = Promise.reject(new Error('session initialization failed'));
  const listeners = process.listenerCount('SIGHUP');
  await assert.rejects(runStandalone(null, []), /session initialization failed/);
  assert.deepEqual(state.events.slice(-2), [['close', 1], ['exit', 1]]);
  assert.equal(process.listenerCount('SIGHUP'), listeners);
  assert.equal(app.listenerCount('before-quit'), 0);
});
