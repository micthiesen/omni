// Fond's service worker. build/precache.sh prepends two constants when trunk
// builds: BUILD (a digest of the app shell) and SHELL (its URLs).
//
// - The shell is precached under a cache named by BUILD and served from it,
//   so an installed app always runs one consistent build, online or not.
// - /img/* and /src/* are immutable: cache-first, kept across builds.
// - /api/* is never touched.

const SHELL_PREFIX = 'fond-shell-';
const SHELL_CACHE = SHELL_PREFIX + BUILD;
const IMAGE_CACHE = 'fond-img-v1';
const APP_ROUTES = [
  /^\/$/,
  /^\/r\/[^/]+\/?$/,
  /^\/cook\/[^/]+\/?$/,
  /^\/inbox\/?$/,
  /^\/requests\/?$/,
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(SHELL_CACHE)
      // `reload` skips the HTTP cache, so a new build never precaches a stale file.
      .then((cache) => cache.addAll(SHELL.map((url) => new Request(url, { cache: 'reload' })))),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      for (const name of await caches.keys()) {
        if (name.startsWith(SHELL_PREFIX) && name !== SHELL_CACHE) await caches.delete(name);
      }
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', (event) => {
  const data = event.data || {};
  if (data.type === 'skip-waiting') self.skipWaiting();
  if (data.type === 'cache-images') event.waitUntil(cacheImages(data.urls || []));
  if (data.type === 'prune-images') event.waitUntil(pruneImages(new Set(data.digests || [])));
});

// Drops /img/<digest>/... files no recipe refers to any more. /src/* stays:
// originals are kept for offline once viewed.
async function pruneImages(digests) {
  const cache = await caches.open(IMAGE_CACHE);
  for (const request of await cache.keys()) {
    const [, root, digest] = new URL(request.url).pathname.split('/');
    if (root === 'img' && !digests.has(digest)) await cache.delete(request);
  }
}

async function cacheImages(urls) {
  const cache = await caches.open(IMAGE_CACHE);
  for (const url of urls) {
    try {
      if (!(await cache.match(url))) {
        const response = await fetchImage(new Request(url));
        if (response.ok) await cache.put(url, response);
      }
    } catch {
      // Offline or gone: the image is fetched again when it is shown.
    }
  }
}

// A long library asks for every cover at once, and the server refuses what
// goes over its limit of requests in flight (503). Images go out a few at a
// time, and a refused or failed one is asked for again.
const IMAGE_FETCHES = 6;
const IMAGE_TRIES = 4;
let imageFetches = 0;
const imageWaiters = [];

async function fetchImage(request) {
  if (imageFetches >= IMAGE_FETCHES) await new Promise((resolve) => imageWaiters.push(resolve));
  else imageFetches += 1;
  try {
    for (let attempt = 1; ; attempt += 1) {
      let response = null;
      try {
        response = await fetch(request.clone());
      } catch (error) {
        if (attempt === IMAGE_TRIES) throw error;
      }
      if (response && (response.status !== 503 || attempt === IMAGE_TRIES)) return response;
      await new Promise((resolve) => setTimeout(resolve, 300 * attempt));
    }
  } finally {
    // A waiter takes the slot over; otherwise it is free again.
    const next = imageWaiters.shift();
    if (next) next();
    else imageFetches -= 1;
  }
}

async function image(request) {
  const cache = await caches.open(IMAGE_CACHE);
  const cached = await cache.match(request.url);
  if (cached) return cached;
  const response = await fetchImage(request);
  if (response.ok) await cache.put(request.url, response.clone());
  return response;
}

async function shell(url) {
  const cache = await caches.open(SHELL_CACHE);
  return (await cache.match(url)) || fetch(url);
}

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin || url.pathname.startsWith('/api/')) return;
  if (url.pathname.startsWith('/img/') || url.pathname.startsWith('/src/')) {
    event.respondWith(image(request));
  } else if (request.mode === 'navigate') {
    if (APP_ROUTES.some((route) => route.test(url.pathname))) event.respondWith(shell('/'));
  } else if (SHELL.includes(url.pathname)) {
    event.respondWith(shell(url.pathname));
  }
});
