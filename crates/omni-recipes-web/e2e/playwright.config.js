// Browser tests for the Fond PWA: WebKit at iPhone size, against any server
// that speaks crates/omni-api/src/fond.rs.
//
//   FOND_BASE_URL   the app's origin (default: the mock server on :8787)
//   FOND_SHOTS      a directory to write light and dark screenshots into
//   see fond.spec.js for the switches that skip tests which leave data behind
const { defineConfig, devices } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  // One library is shared by every test, so they run one at a time, in order.
  workers: 1,
  fullyParallel: false,
  reporter: [['list']],
  outputDir: process.env.FOND_E2E_OUT || 'test-results',
  use: {
    baseURL: process.env.FOND_BASE_URL || 'http://127.0.0.1:8787',
    ...devices['iPhone 14'],
    serviceWorkers: 'allow',
    ignoreHTTPSErrors: false,
    trace: 'off',
  },
  projects: [{ name: 'iphone', use: { ...devices['iPhone 14'] } }],
});
