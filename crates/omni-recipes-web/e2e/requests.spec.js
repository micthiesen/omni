// "Ask Kitchen": a request written in the app is stored, listed on the
// Requests page and answered by an agent with an ordinary revision, and the
// app shows what changed without swapping anything under an open sheet, an
// editor or a cook.
//
// The agent is played through the mock server's /__mock helpers, or with
// FOND_AGENT=claude through the real MCP tools (see agent.js). Against a
// real Omni name a throwaway recipe with FOND_RECIPE_REQUEST: the tests write
// revisions to it and leave their requests closed, marked as test requests.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');
const agent = require('./agent');

// FOND_SHOTS=<dir> writes light and dark screenshots of each new screen.
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

const MOCK = !!process.env.FOND_DIST;
const TITLE = process.env.FOND_RECIPE_REQUEST || 'Chicken Piccata';
// Marks this run's requests, so a shared server's other requests are left alone.
const RUN = `e2e ${Date.now().toString(36)}`;
const TEXT = {
  recipe: `[${RUN}] Test request from the recipe page: please ignore.`,
  cook: `[${RUN}] Test request from cook mode: please ignore.`,
  library: `[${RUN}] Test request from the library: please ignore.`,
  offline: `[${RUN}] Test request written offline: please ignore.`,
};
const DONE_NOTE = 'Test request: handled by the browser tests.';
const DECLINED_NOTE = 'Test request: closed by the browser tests.';

test.describe.configure({ mode: 'serial' });
test.skip(!MOCK && !agent.REAL, 'needs the mock server (FOND_DIST) or FOND_AGENT=claude');
// One real tool call takes a fresh client some twenty seconds.
test.setTimeout(agent.REAL ? 600_000 : 60_000);
const SLOW = { timeout: agent.REAL ? 30_000 : 10_000 };

const card = (page, title) =>
  page.locator('[data-testid=grid] a.card').filter({ has: page.locator('h3', { hasText: title }) });

async function openLibrary(page) {
  await page.goto('/');
  await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
}

async function openRecipe(page) {
  await openLibrary(page);
  await page.getByPlaceholder('Search recipes').fill(TITLE);
  await card(page, TITLE).first().click();
  await expect(page.locator('main.recipe h1')).toHaveText(TITLE);
  return page.url().split('/').pop();
}

async function flushed(page) {
  await expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
}

// Opens the overflow menu of the screen and the Ask sheet from it.
async function openAsk(page) {
  await page.getByRole('button', { name: 'More', exact: true }).click();
  const menu = page.locator('.sheet.open');
  await expect(menu.locator('[data-testid=ask-kitchen]')).toHaveText('Ask Kitchen…');
  await expect(menu.locator('[data-testid=open-requests]')).toHaveText('Requests');
  await shot(page, `requests-menu-${await page.locator('main.page').getAttribute('class').then((c) => c.split(' ').pop())}`);
  await menu.locator('[data-testid=ask-kitchen]').click();
  const sheet = page.locator('.sheet.open');
  await expect(sheet.locator('h2')).toHaveText('Ask Kitchen');
  return sheet;
}

async function submit(page, sheet, text) {
  // Nothing but a text field and Submit, which needs some text.
  await expect(sheet.locator('textarea')).toHaveCount(1);
  await expect(sheet.locator('input, select')).toHaveCount(0);
  await expect(sheet.locator('[data-testid=ask-submit]')).toBeDisabled();
  await sheet.locator('[data-testid=ask-text]').fill(text);
  await shot(page, `requests-ask-${text.match(/from (?:the )?(\w+)/)[1]}`);
  await sheet.locator('[data-testid=ask-submit]').click();
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await expect(page.locator('[data-testid=toast]')).toContainText('Sent to Kitchen');
  await flushed(page);
}

const mine = async (request, status = 'all') =>
  (await agent.requests(request, status)).filter((found) => found.text.includes(`[${RUN}]`));

const item = (page, text) => page.locator('[data-testid=requests] li').filter({ hasText: text });

const summaryOf = (page) => page.locator('main.recipe .summary');

async function discardCook(page) {
  await page.locator('[data-testid=leave]').click();
  await page.locator('[data-testid=leave-discard]').click();
  await page.locator('[data-testid=leave-discard-confirm]').click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
}

let recipeId;
const ids = {};

test('a request from the recipe page carries the recipe, its revision and the scale', async ({ page, request }) => {
  recipeId = await openRecipe(page);
  await page.locator('.scale .seg').getByRole('button', { name: '2', exact: true }).click();
  const sheet = await openAsk(page);
  await expect(sheet.locator('[data-testid=ask-about]')).toContainText(TITLE);
  await submit(page, sheet, TEXT.recipe);
  // Still on the recipe: no conversation opens.
  await expect(page.locator('main.recipe h1')).toHaveText(TITLE);

  const stored = (await mine(request, 'queued')).find((found) => found.text === TEXT.recipe);
  expect(stored).toBeTruthy();
  ids.recipe = stored.id;
  expect(stored.origin).toBe('recipe');
  expect(stored.recipeId).toBe(recipeId);
  expect(stored.scale).toBe(2);
  const head = (await (await request.get('/api/sync')).json()).recipes.find((r) => r.id === recipeId).revision;
  expect(stored.revision).toBe(head);
  expect(stored.step == null).toBe(true);
  await page.locator('.scale .seg').getByRole('button', { name: '1', exact: true }).click();
});

test('a request from cook mode also carries the step that is open', async ({ page, request }) => {
  await openRecipe(page);
  await page.locator('[data-testid=cook]').click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  await page.getByRole('tab', { name: 'Steps' }).click();
  const step = await page.locator('.card-step').getAttribute('data-step');
  const heading = await page.locator('.card-step h2').innerText();
  const sheet = await openAsk(page);
  await expect(sheet.locator('[data-testid=ask-about]')).toContainText(TITLE);
  await expect(sheet.locator('[data-testid=ask-about]')).toContainText(heading.split(': ').pop(), { ignoreCase: true });
  await submit(page, sheet, TEXT.cook);
  // The cook goes on where it was.
  await expect(page.locator('.card-step')).toHaveAttribute('data-step', step);

  const stored = (await mine(request, 'queued')).find((found) => found.text === TEXT.cook);
  expect(stored).toBeTruthy();
  ids.cook = stored.id;
  expect(stored.origin).toBe('cook');
  expect(stored.recipeId).toBe(recipeId);
  expect(stored.step).toBe(step);
  expect(stored.scale).toBe(1);
  await discardCook(page);
  await flushed(page);
});

test('a request from the library names no recipe', async ({ page, request }) => {
  await openLibrary(page);
  const sheet = await openAsk(page);
  await expect(sheet.locator('[data-testid=ask-about]')).toContainText('library');
  await submit(page, sheet, TEXT.library);
  const stored = (await mine(request, 'queued')).find((found) => found.text === TEXT.library);
  expect(stored).toBeTruthy();
  ids.library = stored.id;
  expect(stored.origin).toBe('library');
  expect(stored.recipeId == null).toBe(true);
});

test('the Requests page lists every request as queued, and only lists', async ({ page }) => {
  await openLibrary(page);
  await page.getByRole('button', { name: 'More', exact: true }).click();
  await page.locator('.sheet.open [data-testid=open-requests]').click();
  await expect(page).toHaveURL(/\/requests$/);
  await expect(page.locator('main.requests h1')).toHaveText('Requests');
  for (const text of [TEXT.recipe, TEXT.cook, TEXT.library]) {
    await expect(item(page, text)).toHaveCount(1);
    await expect(item(page, text).locator('[data-testid=request-status]')).toHaveText('Queued');
  }
  // Newest first.
  const texts = await page.locator('[data-testid=requests] li .request-text').allInnerTexts();
  const order = [TEXT.library, TEXT.cook, TEXT.recipe].map((text) => texts.indexOf(text));
  expect(order).toEqual([...order].sort((a, b) => a - b));
  // A request about a recipe links to it; a library request has nothing to link.
  await expect(item(page, TEXT.recipe).getByRole('link', { name: TITLE })).toHaveAttribute('href', `/r/${recipeId}`);
  await expect(item(page, TEXT.library).getByRole('link')).toHaveCount(0);
  // Read-only: no reply field, no buttons on a request, no thread to open.
  await expect(page.locator('[data-testid=requests] button, [data-testid=requests] textarea')).toHaveCount(0);
  await shot(page, 'requests-page-queued');
  // The address opens the page directly.
  await page.reload();
  await expect(item(page, TEXT.recipe)).toHaveCount(1);
});

test('an agent takes a request, writes a revision and closes it; the app follows by itself', async ({ page, request }) => {
  await page.goto('/requests');
  const row = item(page, TEXT.recipe);
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Queued');

  // Working shows without a reload.
  await agent.update(request, { requestId: ids.recipe, status: 'working' });
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Working', SLOW);
  expect((await mine(request, 'queued')).some((found) => found.id === ids.recipe)).toBe(false);

  // The recipe page shows the agent's revision without a reload.
  await row.getByRole('link', { name: TITLE }).click();
  await expect(page.locator('main.recipe h1')).toHaveText(TITLE);
  const summary = `Summary written by the browser tests (${RUN}, first).`;
  const revision = await agent.revise(request, recipeId, summary);
  await expect(summaryOf(page)).toHaveText(summary, SLOW);
  await expect(page.locator('[data-testid=stale]')).toHaveCount(0);

  // Done, with a note and the revision that answers it.
  await agent.update(request, {
    requestId: ids.recipe,
    status: 'done',
    resultRevision: revision,
    note: DONE_NOTE,
  });
  await page.getByRole('button', { name: 'More', exact: true }).click();
  await page.locator('.sheet.open [data-testid=open-requests]').click();
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Done', SLOW);
  await expect(row.locator('[data-testid=request-note]')).toHaveText(DONE_NOTE);
  await shot(page, 'requests-page-done');
  const result = row.getByRole('link', { name: `See revision ${revision}` });
  await expect(result).toHaveAttribute('href', `/r/${recipeId}`);
  await result.click();
  await expect(summaryOf(page)).toHaveText(summary);
});

test('under an open sheet or a focused editor the recipe waits behind "Recipe updated · Reload"', async ({ page, request }) => {
  await openRecipe(page);
  const before = await summaryOf(page).innerText();
  const notice = page.locator('[data-testid=stale]');

  // A sheet is open: nothing changes under it, and the notice sits above it.
  await page.getByRole('button', { name: 'More', exact: true }).click();
  const menu = page.locator('.sheet.open');
  await expect(menu).toBeVisible();
  const second = `Summary written by the browser tests (${RUN}, second).`;
  await agent.revise(request, recipeId, second);
  await expect(notice).toContainText('Recipe updated', SLOW);
  await expect(notice.getByRole('button', { name: 'Reload' })).toBeVisible();
  await expect(summaryOf(page)).toHaveText(before);
  await shot(page, 'requests-stale-under-sheet');
  const [noticeBox, sheetBox] = [await notice.boundingBox(), await menu.boundingBox()];
  expect(noticeBox.y + noticeBox.height).toBeLessThanOrEqual(sheetBox.y);
  // Reload shows the new revision; the sheet stays as it was.
  await notice.getByRole('button', { name: 'Reload' }).click();
  await expect(summaryOf(page)).toHaveText(second);
  await expect(notice).toHaveCount(0);
  await expect(menu).toBeVisible();

  // Left alone, the change arrives when the sheet closes.
  const third = `Summary written by the browser tests (${RUN}, third).`;
  await agent.revise(request, recipeId, third);
  await expect(notice).toContainText('Recipe updated', SLOW);
  await expect(summaryOf(page)).toHaveText(second);
  await menu.getByRole('button', { name: 'Close' }).click();
  await expect(summaryOf(page)).toHaveText(third);
  await expect(notice).toHaveCount(0);

  // The notes field has the keyboard: the page is not redrawn under it.
  const notes = page.getByLabel('Notes');
  await notes.focus();
  const fourth = `Summary written by the browser tests (${RUN}, fourth).`;
  await agent.revise(request, recipeId, fourth);
  await expect(notice).toContainText('Recipe updated', SLOW);
  await expect(summaryOf(page)).toHaveText(third);
  await expect(notes).toBeFocused();
  await shot(page, 'requests-stale-notes');
  await notes.blur();
  await expect(summaryOf(page)).toHaveText(fourth);
  await expect(notice).toHaveCount(0);
});

test('cook mode keeps the revision being cooked and offers "Recipe updated · Reload"', async ({ page, request }) => {
  await openRecipe(page);
  await page.locator('[data-testid=cook]').click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  const sessionId = page.url().split('/').pop();
  await page.getByRole('tab', { name: 'Steps' }).click();
  await expect(page.locator('.card-step')).toBeVisible();
  await flushed(page);
  const session = async () => (await request.get(`/api/cook/${sessionId}`)).json();
  const started = (await session()).revision;
  const stepText = await page.locator('[data-testid=step-text]').innerText();
  // Put the screen somewhere a redraw would lose.
  await page.locator('[data-testid=scroller]').evaluate((area) => { area.scrollTop = 40; });
  const scrolled = await page.locator('[data-testid=scroller]').evaluate((area) => area.scrollTop);

  const revision = await agent.revise(request, recipeId, `Summary written by the browser tests (${RUN}, during a cook).`);
  expect(revision).toBeGreaterThan(started);
  const notice = page.locator('[data-testid=stale]');
  await expect(notice).toContainText('Recipe updated', SLOW);
  // Nothing moved: the same step, the same place, the same pinned revision.
  await expect(page.locator('[data-testid=step-text]')).toHaveText(stepText);
  expect(await page.locator('[data-testid=scroller]').evaluate((area) => area.scrollTop)).toBe(scrolled);
  expect((await session()).revision).toBe(started);
  await shot(page, 'requests-stale-cook');

  await notice.getByRole('button', { name: 'Reload' }).click();
  await expect(notice).toHaveCount(0);
  await expect.poll(async () => (await session()).revision, SLOW).toBe(revision);
  await discardCook(page);
  await flushed(page);
});

test('a request written offline waits on the phone and is sent on reconnect', async ({ page, context, request }) => {
  await openLibrary(page);
  await page.evaluate(() => navigator.serviceWorker.ready);
  await context.setOffline(true);
  const sheet = await openAsk(page);
  await sheet.locator('[data-testid=ask-text]').fill(TEXT.offline);
  await sheet.locator('[data-testid=ask-submit]').click();
  await expect(page.locator('[data-testid=toast]')).toContainText('back online');
  await page.getByRole('button', { name: 'More', exact: true }).click();
  await page.locator('.sheet.open [data-testid=open-requests]').click();
  const row = item(page, TEXT.offline);
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Waiting to send');
  await shot(page, 'requests-page-offline');
  // Closing and opening the app does not lose it.
  await page.reload();
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Waiting to send');

  await context.setOffline(false);
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect(row.locator('[data-testid=request-status]')).toHaveText('Queued', { timeout: 30_000 });
  await expect(row).toHaveCount(1);
  const stored = (await mine(request, 'queued')).filter((found) => found.text === TEXT.offline);
  // Sent once, however often the phone tried.
  expect(stored).toHaveLength(1);
  ids.offline = stored[0].id;
});

test('a declined request shows the note, and the tests leave no request open', async ({ page, request }) => {
  await page.goto('/requests');
  for (const key of ['cook', 'library', 'offline']) {
    await agent.update(request, { requestId: ids[key], status: 'declined', note: DECLINED_NOTE });
  }
  for (const text of [TEXT.cook, TEXT.library, TEXT.offline]) {
    await expect(item(page, text).locator('[data-testid=request-status]')).toHaveText('Declined', SLOW);
    await expect(item(page, text).locator('[data-testid=request-note]')).toHaveText(DECLINED_NOTE);
  }
  await shot(page, 'requests-page-closed');
  expect(await mine(request, 'open')).toEqual([]);
});
