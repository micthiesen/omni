// End-to-end tests of the Fond PWA.
//
// They run against the mock server (dev/mock-server.py) by default and against
// a real Omni with FOND_BASE_URL. Tests that change data put it back (archive
// then unarchive, delete then undo, notes restored). Two leave something a
// server cannot take back, so they can be switched off for production:
//
//   FOND_E2E_SKIP_FINISH=1   do not finish a cook (it is logged in cook history)
//   FOND_E2E_SKIP_INBOX=1    do not upload a photo (an agent would import it)
//
// The only assumptions about the library are named here.
const RECIPES = {
  // A recipe with several components, so its chart has several blocks.
  chart: process.env.FOND_RECIPE_CHART || 'Lasagne alla Bolognese',
  // A recipe to scale and cook; any recipe with at least three steps.
  cook: process.env.FOND_RECIPE_COOK || 'Chicken Piccata',
};

const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const SHOTS = process.env.FOND_SHOTS;

async function shot(page, name) {
  if (!SHOTS) return;
  fs.mkdirSync(SHOTS, { recursive: true });
  for (const scheme of ['light', 'dark']) {
    await page.emulateMedia({ colorScheme: scheme });
    await page.waitForTimeout(150);
    await page.screenshot({ path: path.join(SHOTS, `${name}-${scheme}.png`) });
  }
  await page.emulateMedia({ colorScheme: 'light' });
}

const card = (page, title) =>
  page.locator('[data-testid=grid] a.card').filter({ has: page.locator('h3', { hasText: title }) });

async function openLibrary(page) {
  await page.goto('/');
  await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
}

async function openRecipe(page, title) {
  await openLibrary(page);
  await page.getByPlaceholder('Search recipes').fill(title);
  await card(page, title).first().click();
  await expect(page.locator('main.recipe h1')).toHaveText(title);
}

// Omni refuses writes that do not come from the app's own origin.
const sameOrigin = (page) => ({ origin: new URL(page.url()).origin });

// Everything queued has reached the server.
async function flushed(page) {
  await expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
}

function tinyPng() {
  // A 2x2 red PNG.
  return Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEklEQVR4nGP8z8DAwMDAxAAGAA0GAQEVX6CUAAAAAElFTkSuQmCC',
    'base64',
  );
}

test('the shell is installable: manifest, icons and service worker', async ({ page, request }) => {
  const manifest = await request.get('/manifest.webmanifest');
  expect(manifest.headers()['content-type']).toContain('application/manifest+json');
  const body = await manifest.json();
  expect(body).toMatchObject({ id: '/', scope: '/', start_url: '/', display: 'standalone', name: 'Fond' });
  expect(body.icons.some((icon) => icon.purpose === 'maskable')).toBe(true);
  for (const icon of body.icons) {
    const response = await request.get(icon.src);
    expect(response.status(), icon.src).toBe(200);
    expect(response.headers()['content-type']).toContain('image/png');
  }
  const worker = await request.get('/sw.js');
  expect(worker.headers()['content-type']).toMatch(/javascript/);
  expect(await worker.text()).toContain('const SHELL = [');

  await openLibrary(page);
  const touch = await page.locator('link[rel=apple-touch-icon]').getAttribute('href');
  const touchIcon = await request.get(touch);
  expect(touchIcon.headers()['content-type']).toContain('image/png');
  await expect(page.locator('meta[name=viewport]')).toHaveAttribute('content', /viewport-fit=cover/);
  await expect(page.locator('meta[name=apple-mobile-web-app-capable]')).toHaveAttribute('content', 'yes');
  const registered = await page.evaluate(async () => !!(await navigator.serviceWorker.ready).active);
  expect(registered).toBe(true);
});

test('nothing the app does is refused by the content security policy', async ({ page }) => {
  const refused = [];
  page.on('console', (message) => {
    if (/content security policy|refused to/i.test(message.text())) refused.push(message.text());
  });
  page.on('pageerror', (error) => refused.push(String(error)));
  await page.addInitScript(() => {
    window.__csp = [];
    document.addEventListener('securitypolicyviolation', (event) => {
      window.__csp.push(`${event.violatedDirective} ${event.blockedURI}`);
    });
  });
  const violations = async () => refused.concat(await page.evaluate(() => window.__csp));
  await openRecipe(page, RECIPES.cook);
  await page.getByRole('tab', { name: 'Chart' }).click();
  await page.locator('[data-testid=chart] td.step').first().click();
  await expect(page.locator('.sheet.open .step-text')).not.toBeEmpty();
  expect(await violations()).toEqual([]);
  await page.goto('/inbox');
  await expect(page.getByRole('heading', { name: 'Inbox' })).toBeVisible();
  await page.evaluate(() => navigator.serviceWorker.ready);
  expect(await violations()).toEqual([]);
});

test('the library shows photo cards and typographic tiles', async ({ page }) => {
  await openLibrary(page);
  const cards = page.locator('[data-testid=grid] a.card');
  const count = await cards.count();
  expect(count).toBeGreaterThan(0);
  for (let n = 0; n < count; n++) {
    const image = cards.nth(n).locator('.cover img');
    if (await image.count()) {
      await cards.nth(n).scrollIntoViewIfNeeded();
      await expect.poll(() => image.evaluate((img) => img.naturalWidth)).toBeGreaterThan(0);
    } else {
      await expect(cards.nth(n).locator('.cover.tile span')).not.toBeEmpty();
    }
  }
  // Two columns on a phone.
  const [first, second] = [await cards.nth(0).boundingBox(), await cards.nth(1).boundingBox()];
  if (second) expect(Math.abs(first.y - second.y)).toBeLessThan(4);
  await page.evaluate(() => window.scrollTo(0, 0));
  await shot(page, 'library');
});

test('the library scroll position is restored on the way back', async ({ page }) => {
  // A short screen, so even a small library scrolls.
  await page.setViewportSize({ width: 390, height: 320 });
  await openLibrary(page);
  const room = await page.evaluate(() => document.documentElement.scrollHeight - window.innerHeight);
  test.skip(room < 60, 'the library is too short to scroll');
  await page.evaluate(() => window.scrollTo(0, 60));
  const target = page.locator('[data-testid=grid] a.card').nth(1);
  // Clicking scrolls the card into view first; what counts is where that leaves the page.
  await target.scrollIntoViewIfNeeded();
  const saved = await page.evaluate(() => window.scrollY);
  expect(saved).toBeGreaterThan(0);
  await target.click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
  await page.locator('header').getByRole('button', { name: 'Back' }).click();
  await expect(page.locator('[data-testid=grid]')).toBeVisible();
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(saved);
  // The same through the browser's own back (the iOS swipe).
  await target.click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
  await page.goBack();
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(saved);
});

test('search is instant, prefix-matched and ignores accents and case', async ({ page }) => {
  await openLibrary(page);
  const search = page.getByPlaceholder('Search recipes');
  const words = RECIPES.chart.normalize('NFD').replace(/[̀-ͯ]/g, '').toUpperCase().split(/\s+/);
  await search.fill(words.map((word) => word.slice(0, 4)).join(' '));
  await expect(card(page, RECIPES.chart)).toBeVisible();
  await search.fill('zzzqqq');
  await expect(page.locator('[data-testid=grid] a.card')).toHaveCount(0);
  await expect(page.getByText('No recipes match.')).toBeVisible();
  await search.fill('');
  await expect(card(page, RECIPES.chart)).toBeVisible();
});

test('the filter sheet narrows by tag and counts match', async ({ page }) => {
  await openLibrary(page);
  const all = await page.locator('[data-testid=grid] a.card').count();
  await page.getByRole('button', { name: 'Filter' }).click();
  const chip = page.locator('.sheet.open .chip').first();
  await expect(chip).toBeVisible();
  await shot(page, 'filter');
  const expected = Number(await chip.locator('small').innerText());
  await chip.click();
  await expect(chip).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('button', { name: /^Show \d+ recipes$/ }).click();
  await expect(page.locator('[data-testid=grid] a.card')).toHaveCount(expected);
  await page.getByRole('button', { name: 'Filter' }).click();
  await page.getByRole('button', { name: 'Clear' }).click();
  await page.getByRole('button', { name: /^Show \d+ recipes$/ }).click();
  await expect(page.locator('[data-testid=grid] a.card')).toHaveCount(all);
});

test('a recipe reads as steps and as a chart with merged cells', async ({ page }) => {
  await openRecipe(page, RECIPES.chart);
  await expect(page.locator('.list .ingredients li').first()).toBeVisible();
  expect(await page.locator('.list .step').count()).toBeGreaterThan(2);
  await expect(page.locator('.list .step b').first()).toBeVisible();
  await shot(page, 'recipe-steps');

  await page.getByRole('tab', { name: 'Chart' }).click();
  const chart = page.locator('[data-testid=chart]');
  expect(await chart.locator('table.chart').count()).toBeGreaterThan(1);
  // Step cells really span the rows of what they combine.
  const spans = await chart.locator('td.step[rowspan]').evaluateAll((cells) => cells.map((cell) => cell.rowSpan));
  expect(Math.max(...spans)).toBeGreaterThanOrEqual(3);
  expect(await chart.locator('td[colspan]').count()).toBeGreaterThan(0);
  // Each table is tiled: every row's cells cover the same number of columns.
  const tiled = await chart.locator('table.chart').evaluateAll((tables) =>
    tables.every((table) => {
      const filled = [];
      [...table.rows].forEach((row, r) => {
        let c = 0;
        for (const cell of row.cells) {
          while (filled[r]?.[c]) c++;
          for (let dr = 0; dr < cell.rowSpan; dr++) {
            for (let dc = 0; dc < cell.colSpan; dc++) (filled[r + dr] ??= [])[c + dc] = true;
          }
          c += cell.colSpan;
        }
      });
      const width = filled[0].length;
      return filled.length === table.rows.length && filled.every((row) => row.length === width && !row.includes(undefined));
    }),
  );
  expect(tiled).toBe(true);
  // Tall step cells are set vertically; the ingredient column is sticky.
  const vertical = chart.locator('td.step.vertical span').first();
  expect(await vertical.evaluate((span) => getComputedStyle(span).writingMode)).toBe('vertical-rl');
  expect(await chart.locator('td.lead').first().evaluate((cell) => getComputedStyle(cell).position)).toBe('sticky');
  await chart.locator('table.chart').first().scrollIntoViewIfNeeded();
  await shot(page, 'recipe-chart');

  await chart.locator('td.step').first().click();
  const sheet = page.locator('.sheet.open');
  await expect(sheet.locator('.step-text')).not.toBeEmpty();
  await expect(sheet.locator('.step-no')).toHaveText(/^Step \d+ of \d+$/);
  await shot(page, 'recipe-chart-step');
  await sheet.getByRole('button', { name: 'Close' }).click();
  await expect(page.locator('.sheet.open')).toHaveCount(0);
});

test('a wide chart scrolls sideways under a pinned ingredient column, and turns to landscape', async ({ page }) => {
  await openRecipe(page, RECIPES.chart);
  await page.getByRole('tab', { name: 'Chart' }).click();
  const scrollers = page.locator('[data-testid=chart] .chart-scroll');
  await expect(scrollers.first()).toBeVisible();
  // No chart may widen the page itself.
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  for (let n = 0; n < (await scrollers.count()); n++) {
    const pinned = await scrollers.nth(n).evaluate((scroller) => {
      if (scroller.scrollWidth <= scroller.clientWidth) return null;
      scroller.scrollLeft = scroller.scrollWidth;
      const lead = scroller.querySelector('td.lead').getBoundingClientRect();
      const last = [...scroller.querySelectorAll('td.step')].pop().getBoundingClientRect();
      const box = scroller.getBoundingClientRect();
      return { lead: Math.round(lead.left - box.left), lastVisible: last.right <= box.right + 1 };
    });
    if (pinned) expect(pinned).toEqual({ lead: 0, lastVisible: true });
  }
  await page.setViewportSize({ width: 844, height: 390 });
  await scrollers.first().scrollIntoViewIfNeeded();
  await expect(page.locator('[data-testid=chart] td.step').first()).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await shot(page, 'recipe-chart-landscape');
});

test('scaling changes every amount in both views and is remembered', async ({ page }) => {
  await openRecipe(page, RECIPES.cook);
  await page.locator('.scale .seg button', { hasText: /^1$/ }).click();
  const ingredients = page.locator('.list .ingredients');
  const servings = page.locator('[data-testid=servings] b');
  const before = await ingredients.first().innerText();
  const base = Number(await servings.innerText());
  await expect(page.locator('.scale-note')).toHaveCount(0);

  await page.locator('.scale .seg button', { hasText: /^2$/ }).click();
  await expect(servings).toHaveText(String(base * 2));
  await expect(page.locator('.scale-note')).toContainText('do not scale');
  const doubled = await ingredients.first().innerText();
  expect(doubled).not.toBe(before);
  await page.evaluate(() => window.scrollTo(0, 0));
  await shot(page, 'recipe-scaled');

  await page.getByRole('tab', { name: 'Chart' }).click();
  const chartAtTwo = await page.locator('[data-testid=chart] td.lead').allInnerTexts();
  await page.locator('.scale .seg button', { hasText: /^1$/ }).click();
  await expect.poll(() => page.locator('[data-testid=chart] td.lead').allInnerTexts()).not.toEqual(chartAtTwo);

  await page.getByRole('button', { name: 'More servings' }).click();
  await expect(servings).toHaveText(String(base + 1));
  await page.reload();
  await expect(page.locator('main.recipe h1')).toHaveText(RECIPES.cook);
  await expect(page.locator('[data-testid=servings] b')).toHaveText(String(base + 1));
  await page.locator('.scale .seg button', { hasText: /^1$/ }).click();
  await expect(page.locator('[data-testid=servings] b')).toHaveText(String(base));
});

test('notes save as you type and survive a reload', async ({ page }) => {
  await openRecipe(page, RECIPES.cook);
  const notes = page.getByLabel('Notes');
  await notes.scrollIntoViewIfNeeded();
  const original = await notes.inputValue();
  const added = `${original}${original ? '\n' : ''}e2e ${Date.now()}`;
  await notes.fill(added);
  await notes.blur();
  await flushed(page);
  await page.reload();
  await expect(page.getByLabel('Notes')).toHaveValue(added);
  await page.getByLabel('Notes').scrollIntoViewIfNeeded();
  await shot(page, 'recipe-notes');
  await page.getByLabel('Notes').fill(original);
  await page.getByLabel('Notes').blur();
  await flushed(page);
  await page.reload();
  await expect(page.getByLabel('Notes')).toHaveValue(original);
});

test('the original is one tap away', async ({ page }) => {
  await openRecipe(page, RECIPES.cook);
  const original = page.locator('.original');
  test.skip((await original.count()) === 0, 'this recipe has no stored original');
  await original.getByRole('button').first().click();
  await expect(original.locator('.ext, .src-text, .src-photo').first()).toBeVisible();
});

test('archive and delete each offer an undo', async ({ page }) => {
  await openRecipe(page, RECIPES.chart);
  const url = page.url();

  await page.locator('header').getByRole('button', { name: 'More', exact: true }).click();
  await page.getByRole('button', { name: 'Archive', exact: true }).click();
  await expect(page.locator('[data-testid=toast]')).toContainText('Archived');
  await shot(page, 'toast-undo');
  await expect(card(page, RECIPES.chart)).toHaveCount(0);
  await page.locator('[data-testid=toast]').getByRole('button', { name: 'Undo' }).click();
  await expect(card(page, RECIPES.chart)).toBeVisible();

  // Archive for real, find it behind the Archived switch, and bring it back.
  await page.goto(url);
  await page.locator('header').getByRole('button', { name: 'More', exact: true }).click();
  await page.getByRole('button', { name: 'Archive', exact: true }).click();
  await flushed(page);
  await page.reload();
  await expect(page.locator('[data-testid=grid]')).toBeVisible();
  // Archived recipes are hidden while the switch is off, which is the default.
  await page.getByRole('button', { name: 'Filter' }).click();
  await expect(page.getByRole('switch')).not.toBeChecked();
  await page.locator('.sheet.open').getByRole('button', { name: 'Close' }).click();
  await expect(card(page, RECIPES.chart)).toHaveCount(0);
  await expect(page.locator('[data-testid=grid] .badge', { hasText: 'Archived' })).toHaveCount(0);
  // Someone else (an agent) un-archives it: it is back after the sync, unasked.
  const recipeId = url.split('/').pop();
  const stamp = Date.now();
  const op = (state) => ({ id: `op_e2e${state.slice(0, 3)}${String(stamp).slice(-6)}`, at: Date.now(), type: 'state', recipeId, state });
  expect((await page.request.post('/api/ops', { headers: sameOrigin(page), data: { ops: [op('active')] } })).ok()).toBe(true);
  await expect(card(page, RECIPES.chart)).toBeVisible();
  expect((await page.request.post('/api/ops', { headers: sameOrigin(page), data: { ops: [op('archived')] } })).ok()).toBe(true);
  await expect(card(page, RECIPES.chart)).toHaveCount(0);
  await page.getByRole('button', { name: 'Filter' }).click();
  await page.getByRole('switch').check();
  await page.getByRole('button', { name: /^Show \d+ recipes$/ }).click();
  await expect(card(page, RECIPES.chart).locator('.badge')).toHaveText('Archived');
  await card(page, RECIPES.chart).click();
  await page.locator('header').getByRole('button', { name: 'More', exact: true }).click();
  await page.getByRole('button', { name: 'Unarchive' }).click();
  await flushed(page);

  // Delete, then undo within the ten seconds.
  await page.goto(url);
  await page.locator('header').getByRole('button', { name: 'More', exact: true }).click();
  await page.getByRole('button', { name: 'Delete' }).click();
  await expect(page.locator('[data-testid=toast]')).toContainText('Deleted');
  await expect(card(page, RECIPES.chart)).toHaveCount(0);
  await flushed(page);
  await page.locator('[data-testid=toast]').getByRole('button', { name: 'Undo' }).click();
  await expect(card(page, RECIPES.chart)).toBeVisible();
  await flushed(page);
  await page.reload();
  await expect(card(page, RECIPES.chart)).toBeVisible();
});

test('cook mode: cross off in all three tabs, adjust, timer, reload, finish', async ({ page }) => {
  await openRecipe(page, RECIPES.cook);
  await page.locator('[data-testid=cook]').click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  const cookUrl = page.url();

  // Steps: tap an input chip, then Done moves on.
  const stepCard = page.locator('.card-step');
  const firstStep = await stepCard.getAttribute('data-step');
  const chip = stepCard.locator('.chip.input').first();
  await chip.click();
  await expect(chip).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.cook .step-text')).toHaveCSS('font-size', '20px');
  await shot(page, 'cook-steps');
  await page.locator('[data-testid=done]').click();
  await expect(stepCard).not.toHaveAttribute('data-step', firstStep);
  const secondStep = await stepCard.getAttribute('data-step');
  await expect(page.locator('[data-testid=next]')).toContainText(/Next|last step|Nothing/);

  // Adjust the step for this cook only.
  const adjusted = `Adjusted by e2e ${Date.now()}`;
  await page.locator('[data-testid=adjust]').click();
  await page.getByLabel('Adjusted text').fill(adjusted);
  await shot(page, 'cook-adjust');
  await page.locator('[data-testid=save-adjust]').click();
  await expect(page.locator('[data-testid=step-text]')).toHaveText(adjusted);
  await expect(stepCard.locator('.adjusted .mark')).toBeVisible();
  await stepCard.getByRole('button', { name: 'Original', exact: true }).click();
  await expect(stepCard.locator('.orig')).not.toBeEmpty();

  // Timer, when this step has a time.
  const timer = page.locator('[data-testid=timer]');
  if (await timer.count()) {
    await timer.click();
    await expect(page.locator('[data-testid=countdown]')).toHaveText(/^\d+:\d\d(:\d\d)?$/);
    await expect(page.locator('[data-testid=running]')).toBeVisible();
    await shot(page, 'cook-timer');
  }

  // A timer set from elsewhere (an agent, another device) arrives over the
  // event stream and rings here when it runs out.
  const sessionId = cookUrl.split('/').pop();
  const pushed = await page.request.post(`/api/cook/${sessionId}/events`, {
    headers: sameOrigin(page),
    data: {
      events: [
        { seq: 0, clientEventId: `ev_e2e${Date.now()}`, at: Date.now(), actor: 'app', type: 'timer', step: firstStep, endsAt: Date.now() + 2500 },
      ],
    },
  });
  expect(pushed.ok()).toBe(true);
  const alert = page.locator('[data-testid=timer-alert]');
  await expect(alert).toContainText('Timer done');
  await shot(page, 'cook-timer-alert');
  await alert.getByRole('button', { name: 'Stop' }).click();
  await expect(alert).toHaveCount(0);

  // Gather: check an ingredient off.
  await page.getByRole('tab', { name: 'Gather' }).click();
  const item = page.locator('.gather .check').first();
  const itemId = await item.getAttribute('data-target');
  await item.click();
  await expect(item).toHaveClass(/done/);
  await shot(page, 'cook-gather');

  // Chart: the done step is dimmed, the current one outlined; tap to cross off.
  await page.getByRole('tab', { name: 'Chart' }).click();
  const chart = page.locator('[data-testid=chart]');
  await expect(chart.locator(`td.step[data-key="${firstStep}"], td.setup[data-key="${firstStep}"]`).first()).toHaveClass(/done/);
  await expect(chart.locator('td.ready').first()).toBeVisible();
  const leaf = chart.locator(`td.lead[data-key^="${secondStep}/"]`).first();
  await leaf.click();
  await expect(leaf).toHaveClass(/done/);
  const leafKey = await leaf.getAttribute('data-key');
  const stepCell = chart.locator(`td.step[data-key="${secondStep}"]`);
  await stepCell.click();
  await expect(stepCell).toHaveClass(/done/);
  await shot(page, 'cook-chart');
  await stepCell.click();
  await expect(stepCell).not.toHaveClass(/done/);

  // Everything is in the event log: a reload brings it all back.
  await flushed(page);
  await page.goto(cookUrl);
  await expect(page.locator('.card-step')).toHaveAttribute('data-step', secondStep);
  await expect(page.locator('[data-testid=step-text]')).toHaveText(adjusted);
  if (await page.locator('[data-testid=countdown]').count()) {
    await expect(page.locator('[data-testid=countdown]')).toHaveText(/^\d+:\d\d(:\d\d)?$/);
    await page.locator('.card-step').getByRole('button', { name: 'Cancel' }).click();
    await expect(page.locator('[data-testid=timer]')).toBeVisible();
  }
  await page.getByRole('tab', { name: 'Gather' }).click();
  await expect(page.locator(`.gather .check[data-target="${itemId}"]`)).toHaveClass(/done/);
  await page.getByRole('tab', { name: 'Chart' }).click();
  await expect(page.locator(`[data-testid=chart] td.lead[data-key="${leafKey}"]`)).toHaveClass(/done/);

  // The library lists the open session.
  await page.goto('/');
  await expect(page.locator('.cooking-card').filter({ hasText: RECIPES.cook })).toContainText(/step \d+ of \d+/);
  await shot(page, 'library-cooking');
  await page.locator('.cooking-card').filter({ hasText: RECIPES.cook }).click();
  await expect(page).toHaveURL(cookUrl);

  test.skip(!!process.env.FOND_E2E_SKIP_FINISH, 'finishing a cook is switched off');
  await page.locator('[data-testid=finish]').click();
  const sheet = page.locator('.sheet.open');
  await expect(sheet.locator('.adjusted-list')).toContainText(adjusted);
  await sheet.getByPlaceholder('One line, optional').fill('e2e run');
  await sheet.locator('input[type=file]').setInputFiles({ name: 'dish.png', mimeType: 'image/png', buffer: tinyPng() });
  await shot(page, 'cook-finish');
  await page.locator('[data-testid=finish-confirm]').click();
  await expect(page.locator('main.recipe h1')).toHaveText(RECIPES.cook);
  await flushed(page);
  // The photo of the result joined the recipe's gallery and is served from /img.
  const photos = page.locator('[data-testid=gallery] img');
  await expect.poll(() => photos.count()).toBeGreaterThan(0);
  expect((await photos.first().boundingBox()).height).toBeGreaterThan(200);
  await expect.poll(() => photos.last().getAttribute('src')).toMatch(/^\/img\/[0-9a-f]{64}\/full\.jpg$/);
  const lastPhoto = await photos.last().getAttribute('src');
  expect((await page.request.get(lastPhoto)).headers()['content-type']).toBe('image/jpeg');
  // The adjustment is now a "Last time" callout on its step; dismiss it again.
  const callout = page.locator('.last-time').filter({ hasText: adjusted });
  await expect(callout).toBeVisible();
  await callout.scrollIntoViewIfNeeded();
  await shot(page, 'recipe-last-time');
  await callout.getByRole('button', { name: 'Dismiss' }).click();
  await expect(callout).toHaveCount(0);
  await flushed(page);
  await page.goto('/');
  await expect(page.locator('.cooking-card').filter({ hasText: RECIPES.cook })).toHaveCount(0);
});

test('the inbox uploads photos as one item', async ({ page }) => {
  test.skip(!!process.env.FOND_E2E_SKIP_INBOX, 'inbox uploads are switched off');
  await page.goto('/inbox');
  await expect(page.getByRole('heading', { name: 'Inbox' })).toBeVisible();
  await expect(page.locator('[data-testid=camera-input]')).toHaveAttribute('capture', 'environment');
  const before = await page.locator('[data-testid=inbox-items] li[data-state=pending]').count();
  await page.locator('[data-testid=library-input]').setInputFiles([
    { name: 'page-1.png', mimeType: 'image/png', buffer: tinyPng() },
    { name: 'page-2.png', mimeType: 'image/png', buffer: tinyPng() },
  ]);
  await flushed(page);
  const pending = page.locator('[data-testid=inbox-items] li[data-state=pending]');
  await expect(pending).toHaveCount(before + 1);
  await expect(pending.first()).toContainText('2 photos');
  await expect(pending.first()).toContainText('Waiting for an agent');
  await shot(page, 'inbox');
});

test('offline: the app loads, reads and cooks, and sends on reconnect', async ({ page, context, request }) => {
  // First visit: install the worker, sync the library, view the recipe.
  await openLibrary(page);
  await page.evaluate(() => navigator.serviceWorker.ready);
  await page.reload();
  await expect.poll(() => page.evaluate(() => !!navigator.serviceWorker.controller)).toBe(true);
  await openRecipe(page, RECIPES.cook);
  const recipeUrl = page.url();

  await context.setOffline(true);
  await page.goto('/');
  await expect(card(page, RECIPES.cook)).toBeVisible();
  await expect(page.locator('[data-testid=connection]')).toContainText('Offline');
  await shot(page, 'offline-library');
  await page.goto(recipeUrl);
  await expect(page.locator('main.recipe h1')).toHaveText(RECIPES.cook);
  await page.getByRole('tab', { name: 'Chart' }).click();
  await expect(page.locator('[data-testid=chart] table.chart').first()).toBeVisible();

  // Cook offline: the session starts on the phone.
  await page.locator('[data-testid=cook]').click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  const cookUrl = page.url();
  const sessionId = cookUrl.split('/').pop();
  const firstStep = await page.locator('.card-step').getAttribute('data-step');
  await page.locator('[data-testid=done]').click();
  await expect(page.locator('[data-testid=connection]')).toContainText(/Offline · \d+ to send/);
  await shot(page, 'offline-cook');
  await page.goto(cookUrl);
  await expect(page.locator('.card-step')).not.toHaveAttribute('data-step', firstStep);

  await context.setOffline(false);
  await flushed(page);
  const stored = await (await request.get(`/api/cook/${sessionId}`)).json();
  expect(stored.events.some((event) => event.type === 'check' && event.target === firstStep)).toBe(true);

  // Leave nothing half-cooked behind: undo the check; the idle close ends the session.
  await page.getByRole('button', { name: 'Previous step' }).click();
  await page.getByRole('button', { name: 'Not done' }).click();
  await flushed(page);
});
