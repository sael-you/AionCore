// AionUi's Camoufox sidecar (TASK-136 K6, C35 v2 option A).
//
// Runs AS the unprivileged browser account. Launches Camoufox on the
// instance's Xvfb display with a persistent profile, every request through
// AionCore's egress proxy, then serves that browser (and only it) to AionCore
// on a loopback WebSocket with a random path. AionCore never hands this
// endpoint to agents: they reach it through the bridge's Playwright-protocol
// filter. Prints exactly one JSON line `{"endpoint": "..."}` on
// stdout, then lives until stdin closes (AionCore stopped it or died).
//
// argv[2]: JSON {executable, profile, proxyPort, downloads}
// Pinned: playwright-core 1.62.0-alpha-1783623505000 (from @playwright/cli
// 0.1.17, see package-lock.json): Camoufox pins playwright <1.63 because
// every Playwright minor may change Juggler.

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { firefox } = require('playwright-core');

const cfg = JSON.parse(process.argv[2] ?? '{}');
for (const key of ['executable', 'profile', 'proxyPort', 'downloads']) {
  if (cfg[key] === undefined) {
    console.error(`sidecar: missing ${key}`);
    process.exit(2);
  }
}

const proxy = `127.0.0.1`;
const port = Number(cfg.proxyPort);

// Every connection goes through the egress proxy, loopback included (the
// proxy refuses internal addresses, AionUi's hosts and DNS rebinding).
const firefoxUserPrefs = {
  'network.proxy.type': 1,
  'network.proxy.http': proxy,
  'network.proxy.http_port': port,
  'network.proxy.ssl': proxy,
  'network.proxy.ssl_port': port,
  'network.proxy.share_proxy_settings': true,
  'network.proxy.no_proxies_on': '',
  'network.proxy.allow_hijacking_localhost': true,
  'network.proxy.failover_direct': false,
  'network.proxy.socks_remote_dns': true,
  // No DNS of its own (DoH off, no prefetch) and no direct UDP.
  'network.trr.mode': 5,
  'network.dns.disablePrefetch': true,
  'network.prefetch-next': false,
  'network.predictor.enabled': false,
  'media.peerconnection.ice.proxy_only': true,
  'media.peerconnection.ice.default_address_only': true,
  // Local files stay out of reach of pages and of the address bar.
  'browser.download.dir': cfg.downloads,
  'browser.download.folderList': 2,
};

let context;
let closing = false;

async function shutdown(code) {
  if (closing) return;
  closing = true;
  try {
    await context?.close();
  } catch {
    // The browser may already be gone.
  }
  process.exit(code);
}

process.stdin.on('end', () => void shutdown(0));
process.stdin.on('error', () => void shutdown(0));
process.stdin.resume();
process.on('SIGTERM', () => void shutdown(0));
process.on('SIGINT', () => void shutdown(0));

try {
  context = await firefox.launchPersistentContext(cfg.profile, {
    executablePath: cfg.executable,
    headless: false,
    viewport: null,
    acceptDownloads: true,
    downloadsPath: cfg.downloads,
    proxy: { server: `http://${proxy}:${port}`, bypass: '' },
    firefoxUserPrefs,
    handleSIGINT: false,
    handleSIGTERM: false,
    handleSIGHUP: false,
    env: {
      DISPLAY: process.env.DISPLAY,
      HOME: process.env.HOME,
      LANG: process.env.LANG ?? 'en_US.UTF-8',
      PATH: process.env.PATH,
    },
  });
  const browser = context.browser();
  browser.on('disconnected', () => void shutdown(0));
  const { endpoint } = await browser.bind('aionui', { host: '127.0.0.1', port: 0 });
  process.stdout.write(`${JSON.stringify({ endpoint })}\n`);
} catch (e) {
  console.error(`sidecar: ${e?.message ?? e}`);
  await shutdown(1);
}
