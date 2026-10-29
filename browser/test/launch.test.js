const assert = require('node:assert/strict');
const { test } = require('node:test');
const { parseBrowserArguments } = require('../dist/launch.js');

test('the current terminal daemon entry remains the default', () => {
  assert.deepEqual(parseBrowserArguments([]), { standalone: false, argv: [] });
  assert.deepEqual(parseBrowserArguments(['--daemon', '--ozone-platform=headless', '--screen-info={8192x8192}']), { standalone: false, argv: [] });
});

test('foreground startup preserves URL and browser options without passing Chromium switches to the session', () => {
  assert.deepEqual(parseBrowserArguments(['--ozone-platform=headless', '--standalone', '--transparent', 'https://example.com', '--allow-clipboard-read', '--screen-info={8192x8192}']), {
    standalone: true, argv: ['--transparent', 'https://example.com', '--allow-clipboard-read'],
  });
});

test('conflicting modes and unsupported terminal-only options are rejected', () => {
  assert.throws(() => parseBrowserArguments(['https://example.com']), /--standalone/);
  assert.throws(() => parseBrowserArguments(['--standalone', '--daemon']), /cannot be combined/);
  assert.throws(() => parseBrowserArguments(['--standalone', '--split=right']), /unsupported foreground/);
  assert.throws(() => parseBrowserArguments(['--standalone', '--ssh=host']), /unsupported foreground/);
  assert.throws(() => parseBrowserArguments(['--standalone', 'one', 'two']), /one initial URL/);
});
