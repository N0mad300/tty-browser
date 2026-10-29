const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');
const { entryArguments } = require('../dist/entry.js');

function fixture(t, manifest) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'pixel-entry-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  fs.mkdirSync(path.join(directory, 'dist'));
  fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify(manifest));
  return path.join(directory, 'dist', 'main.js');
}

test('ordinary Pixel apps keep their after-ready bootstrap and argument order', t => {
  const entry = fixture(t, { main: 'dist/main.js' });
  assert.deepEqual(entryArguments(entry, '/pixel/bootstrap.js', ['--ozone-platform=headless', 'https://example.com']), ['/pixel/bootstrap.js', entry, '--ozone-platform=headless', 'https://example.com']);
});

test('application-owned Electron startup runs the real entry before the app is ready', t => {
  const entry = fixture(t, { pixel: { bootstrap: false } });
  assert.deepEqual(entryArguments(entry, '/pixel/bootstrap.js', ['--standalone', 'file:///site.html']), [entry, '--standalone', 'file:///site.html']);
});

test('the closest package controls startup and invalid lifecycle configuration fails clearly', t => {
  const entry = fixture(t, { pixel: { bootstrap: false } });
  fs.writeFileSync(path.join(path.dirname(entry), 'package.json'), JSON.stringify({ pixel: { bootstrap: true } }));
  assert.deepEqual(entryArguments(entry, '/pixel/bootstrap.js', []), ['/pixel/bootstrap.js', entry]);
  fs.writeFileSync(path.join(path.dirname(entry), 'package.json'), JSON.stringify({ pixel: { bootstrap: 'false' } }));
  assert.throws(() => entryArguments(entry, '/pixel/bootstrap.js', []), /pixel.bootstrap must be a boolean/);
});
