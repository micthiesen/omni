// Checks that need the mock server (dev/mock-server.py): they only run when
// FOND_DIST names the dist directory it serves.
//
// - The update flow: a deploy is simulated by changing the build digest in
//   the served sw.js.
// - What an agent does behind the app's back, played through /__mock/*.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const DIST = process.env.FOND_DIST;

test.describe('a new deploy', () => {
  test.skip(!DIST, 'set FOND_DIST to the dist directory the server serves');
  const worker = DIST && path.join(DIST, 'sw.js');
  let original;

  test.beforeEach(async ({ page }) => {
    original = fs.readFileSync(worker, 'utf8');
    await page.goto('/');
    await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
    await page.evaluate(() => navigator.serviceWorker.ready);
    await page.reload();
    await expect.poll(() => page.evaluate(() => !!navigator.serviceWorker.controller)).toBe(true);
  });

  test.afterEach(() => fs.writeFileSync(worker, original));

  function deploy(tag) {
    const build = `${original.match(/const BUILD = "([^"]+)"/)[1]}-${tag}`;
    fs.writeFileSync(worker, original.replace(/const BUILD = "[^"]+"/, `const BUILD = "${build}"`));
    return `fond-shell-${build}`;
  }

  const shells = async (page) => {
    try {
      return (await page.evaluate(() => caches.keys())).filter((name) => name.startsWith('fond-shell-'));
    } catch {
      return null; // the page is reloading
    }
  };

  test('waits behind the open app and takes over on a tap', async ({ page }) => {
    const before = await shells(page);
    const cache = deploy('tap');
    await page.evaluate(async () => (await navigator.serviceWorker.getRegistration()).update());
    await expect(page.getByText('A new version of Fond is ready.')).toBeVisible();
    // Nothing changed under the running app.
    expect(await shells(page)).toEqual(expect.arrayContaining(before));
    await page.getByRole('button', { name: 'Update' }).click();
    await expect.poll(() => shells(page)).toEqual([cache]);
    await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
    await expect(page.getByText('A new version of Fond is ready.')).toHaveCount(0);
  });

  test('takes over at the next launch', async ({ page }) => {
    const cache = deploy('launch');
    await page.evaluate(async () => (await navigator.serviceWorker.getRegistration()).update());
    await expect(page.getByText('A new version of Fond is ready.')).toBeVisible();
    await page.reload();
    await expect.poll(() => shells(page)).toEqual([cache]);
    await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
  });
});

test.describe('the built app', () => {
  test.skip(!DIST, 'set FOND_DIST to the dist directory the server serves');

  // A change made while cooking is a change to the recipe; nothing in the
  // app may say otherwise.
  test('has no wording about changes for one cook only', () => {
    const files = fs.readdirSync(DIST, { recursive: true }).map((name) => path.join(DIST, String(name)));
    const found = files
      .filter((file) => fs.statSync(file).isFile())
      .filter((file) => /this cook only|for this cook|adjusted this time|use again/i.test(fs.readFileSync(file).toString('latin1')));
    expect(found).toEqual([]);
  });

  test('an app from before overrides were retired can still send its events', async ({ page, request }) => {
    await page.goto('/');
    await page.locator('[data-testid=grid] a.card').first().click();
    const recipeId = page.url().split('/').pop();
    const sessionId = `cook_${String(Date.now()).slice(-8)}`;
    await request.post('/api/cook', { data: { id: sessionId, recipeId, revision: 1, scale: 1, at: Date.now() } });
    const event = (id, kind) => ({ seq: 0, clientEventId: `${sessionId}-${id}`, at: Date.now(), actor: 'app', ...kind });
    const sent = await request.post(`/api/cook/${sessionId}/events`, {
      data: { events: [
        event('a', { type: 'adjust', target: 'sear', text: 'Two batches.' }),
        event('b', { type: 'check', target: 'sear' }),
        event('c', { type: 'abandon' }),
      ] },
    });
    expect(sent.ok()).toBe(true);
    const stored = await sent.json();
    expect(stored.events.map((found) => found.type)).toEqual(['check', 'abandon']);
    expect(stored.state).toBe('abandoned');
  });
});

test.describe('an agent at work', () => {
  test.skip(!DIST, 'needs the mock server (set FOND_DIST)');

  test('a revised recipe shows "Recipe updated" and the cook can reload', async ({ page, request }) => {
    await page.goto('/');
    await page.locator('[data-testid=grid] a.card').first().click();
    const recipeId = page.url().split('/').pop();
    await page.locator('[data-testid=cook]').click();
    await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
    const sessionId = page.url().split('/').pop();
    await expect(page.locator('.card-step')).toBeVisible();
    await expect(page.locator('[data-testid=connection]')).toHaveCount(0);
    await request.post('/__mock/revise', { data: { recipeId } });
    const notice = page.locator('[data-testid=stale]');
    await expect(notice).toContainText('Recipe updated');
    await notice.getByRole('button', { name: 'Reload' }).click();
    await expect(notice).toHaveCount(0);
    await expect.poll(async () => (await (await request.get(`/api/cook/${sessionId}`)).json()).revision).toBe(2);

    // A note an agent adds to the session appears on the phone.
    await request.post('/__mock/note', { data: { sessionId, text: 'No buttermilk: use milk and lemon.' } });
    await expect(page.locator('.session-note')).toHaveText('No buttermilk: use milk and lemon.');
  });

  test('a running passive step offers what can be done meanwhile', async ({ page }) => {
    // The mock's enchiladas open with a passive step ("boil down") that the next step does not wait for.
    await page.goto('/');
    await page.locator('[data-testid=grid] a.card').filter({ has: page.locator('h3', { hasText: 'Chicken Enchiladas' }) }).click();
    await page.locator('[data-testid=cook]').click();
    await expect(page.locator('.card-step')).toHaveAttribute('data-step', 'sauce');
    await page.locator('[data-testid=timer]').click();
    await expect(page.locator('[data-testid=running]')).toBeVisible();
    const offers = page.locator('.steps-tab .meanwhile');
    await expect(offers).toContainText('While «boil down 15 min» runs:');
    await offers.locator('.chip', { hasText: 'fry' }).click();
    await expect(page.locator('.card-step')).toHaveAttribute('data-step', 'fry');
  });

  test('a session renders the document the server pinned, and a photo retry is stored once', async ({ page, request }) => {
    await page.goto('/');
    await page.locator('[data-testid=grid] a.card').first().click();
    const recipeId = page.url().split('/').pop();
    const sessionId = `cook_${String(Date.now()).slice(-8)}`;
    const started = await request.post('/api/cook', { data: { id: sessionId, recipeId, revision: 1, scale: 1, at: Date.now() } });
    expect((await started.json()).doc.title).toBeTruthy();
    // Started elsewhere: this phone has no copy of its own, only the server's.
    await page.goto(`/cook/${sessionId}`);
    await expect(page.locator('.card-step [data-testid=step-text]')).not.toBeEmpty();
    const stored = await page.evaluate(
      (id) =>
        new Promise((resolve) => {
          const open = indexedDB.open('fond');
          open.onsuccess = () => {
            const get = open.result.transaction('sessions').objectStore('sessions').get(id);
            get.onsuccess = () => resolve(JSON.parse(get.result));
          };
        }),
      sessionId,
    );
    expect(stored.doc.title).toBeTruthy();

    const images = async () => (await (await request.get('/api/sync')).json()).recipes.find((r) => r.id === recipeId).images.length;
    const before = await images();
    const jpeg = Buffer.from('/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAAA//EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AN//Z', 'base64');
    for (let attempt = 0; attempt < 2; attempt++) {
      const sent = await request.post(`/api/cook/${sessionId}/photo?id=blob_e2e${sessionId}`, {
        headers: { 'x-fond-upload': '1', 'content-type': 'image/jpeg' },
        data: jpeg,
      });
      expect(sent.ok()).toBe(true);
    }
    expect(await images()).toBe(before + 1);
  });

  test('images no recipe refers to any more leave the image cache', async ({ page, request }) => {
    await page.goto('/');
    await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
    await page.evaluate(() => navigator.serviceWorker.ready);
    await page.reload();
    await expect.poll(() => page.evaluate(() => !!navigator.serviceWorker.controller)).toBe(true);
    const withImages = (await (await request.get('/api/sync')).json()).recipes.filter((recipe) => recipe.images.length);
    expect(withImages.length).toBeGreaterThan(1);
    const [gone, kept] = withImages;
    const cached = () =>
      page.evaluate(async () => {
        const cache = await caches.open('fond-img-v1');
        return (await cache.keys()).map((entry) => new URL(entry.url).pathname.split('/')[2]);
      });
    // Every cover thumbnail is cached after the sync.
    await expect.poll(cached).toEqual(expect.arrayContaining([gone.images[0].digest, kept.images[0].digest]));
    await request.post('/__mock/unimage', { data: { recipeId: gone.id } });
    await expect.poll(cached).not.toContain(gone.images[0].digest);
    expect(await cached()).toContain(kept.images[0].digest);
    await request.post('/__mock/reset', { data: {} });
  });

  test('an imported inbox item links to its recipe', async ({ page, request }) => {
    await page.goto('/');
    await page.locator('[data-testid=grid] a.card').first().click();
    const recipeId = page.url().split('/').pop();
    await page.goto('/inbox');
    const png = Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEklEQVR4nGP8z8DAwMDAxAAGAA0GAQEVX6CUAAAAAElFTkSuQmCC',
      'base64',
    );
    await page.locator('[data-testid=library-input]').setInputFiles([{ name: 'card.png', mimeType: 'image/png', buffer: png }]);
    const item = page.locator('[data-testid=inbox-items] li[data-state=pending]').first();
    await expect(item).toContainText('1 photo');
    const itemId = await item.getAttribute('data-item');
    await request.post('/__mock/import', { data: { itemId, recipeId } });
    const imported = page.locator(`[data-testid=inbox-items] li[data-item="${itemId}"]`);
    await expect(imported).toHaveAttribute('data-state', 'imported');
    await imported.getByRole('link', { name: 'Open recipe' }).click();
    await expect(page).toHaveURL(new RegExp(`/r/${recipeId}$`));
  });
});

test.describe('changes that arrive under an open sheet', () => {
  test.skip(!DIST, 'needs the mock server (set FOND_DIST)');

  test('the library keeps its grid under the filter sheet and follows again when it closes', async ({ page, request }) => {
    await page.goto('/');
    const first = page.locator('[data-testid=grid] a.card').first();
    await expect(first).toBeVisible();
    const recipeId = (await first.getAttribute('href')).split('/').pop();
    const title = await first.locator('h3').innerText();
    const notice = page.locator('[data-testid=stale]');

    // With nothing open, a renamed recipe shows by itself.
    await request.post('/__mock/revise', { data: { recipeId, title: `${title} I` } });
    await expect(first.locator('h3')).toHaveText(`${title} I`);
    await expect(notice).toHaveCount(0);

    // Under the filter sheet the grid stays as it is.
    await page.getByRole('button', { name: 'Filter' }).click();
    const sheet = page.locator('.sheet.open');
    await expect(sheet).toBeVisible();
    await request.post('/__mock/revise', { data: { recipeId, title: `${title} II` } });
    await expect(notice).toContainText('Recipes updated');
    await expect(first.locator('h3')).toHaveText(`${title} I`);
    await notice.getByRole('button', { name: 'Reload' }).click();
    await expect(first.locator('h3')).toHaveText(`${title} II`);
    await expect(notice).toHaveCount(0);
    await expect(sheet).toBeVisible();

    await request.post('/__mock/revise', { data: { recipeId, title } });
    await expect(notice).toContainText('Recipes updated');
    await sheet.getByRole('button', { name: 'Close' }).click();
    await expect(first.locator('h3')).toHaveText(title);
    await expect(notice).toHaveCount(0);
  });

  test('the Requests page keeps its list while a request is being written', async ({ page, request }) => {
    await page.goto('/requests');
    await page.locator('main.requests [data-testid=ask-kitchen]').click();
    const sheet = page.locator('.sheet.open');
    await sheet.locator('[data-testid=ask-text]').fill('Add a flatbread recipe.');
    await sheet.locator('[data-testid=ask-submit]').click();
    const row = page.locator('[data-testid=requests] li').filter({ hasText: 'Add a flatbread recipe.' });
    await expect(row.locator('[data-testid=request-status]')).toHaveText('Queued');
    const requestId = await row.getAttribute('data-request');

    await page.locator('main.requests [data-testid=ask-kitchen]').click();
    await expect(sheet).toBeVisible();
    await request.post('/__mock/request', { data: { requestId, status: 'working' } });
    const notice = page.locator('[data-testid=stale]');
    await expect(notice).toContainText('Requests updated');
    await expect(row.locator('[data-testid=request-status]')).toHaveText('Queued');
    await sheet.getByRole('button', { name: 'Cancel' }).click();
    await expect(row.locator('[data-testid=request-status]')).toHaveText('Working');
    await expect(notice).toHaveCount(0);
    await request.post('/__mock/request', { data: { requestId, status: 'declined', note: 'Test.' } });
    await expect(row.locator('[data-testid=request-status]')).toHaveText('Declined');
  });
});
