const assert = require('node:assert/strict');
const { test } = require('node:test');
const { computeLayout } = require('../dist/session/layout.js');

test('browser chrome and WebView layout use physical pixels and scale without a cell grid', () => {
  const info = { width: 1001, height: 701, basePx: 24, cellWidth: 0, cellHeight: 0, scaleFactor: 1.5 };
  const layout = computeLayout(info, 1.5, null);
  assert.equal(layout.chrome.width, 1001);
  assert.equal(layout.chrome.height, 701);
  assert.equal(layout.surface.scale, 1.5);
  assert(layout.surface.width > 900 && layout.surface.width <= 1001);
  assert(layout.surface.height > 600 && layout.surface.height < 701);
  assert(layout.surface.y > 0);
  assert(Number.isFinite(layout.surface.width) && Number.isFinite(layout.surface.height));
  const devtools = computeLayout(info, 1.5, { dock:'bottom', fraction:0.4 });
  assert(devtools.devtools);
  assert(devtools.surface.height < layout.surface.height);
  assert.equal(devtools.devtools.scale, 1.5);
});
