// The library's masonry grid, pinned recipes (pin, unpin, reorder by drag and
// by the menu) and the unit-aware steppers. Runs against the mock server and,
// with FOND_BASE_URL, against a real Omni: it pins the first two recipes of
// the library and unpins them again, and saves no amount.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const SHOTS = process.env.FOND_SHOTS;
const MOCK = !!process.env.FOND_DIST;

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

test.describe.configure({ mode: 'serial' });

const cards = (page, grid = 'grid') => page.locator(`[data-testid=${grid}] .card-wrap`);
const flushed = (page) => expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
const sync = async (page) => (await page.request.get('/api/sync')).json();
const sameOrigin = (page) => ({ origin: new URL(page.url()).origin });

async function openLibrary(page) {
  await page.goto('/');
  await expect(cards(page).first()).toBeVisible();
}

// Every card of a grid with its place on screen: column (by left edge) and box.
const layout = (page, grid = 'grid') =>
  page.locator(`[data-testid=${grid}]`).evaluate((root) => {
    const columns = [...root.querySelectorAll('.grid-col')].map((col) => Math.round(col.getBoundingClientRect().left));
    return [...root.querySelectorAll('.card-wrap')].map((card) => {
      const box = card.getBoundingClientRect();
      const title = card.querySelector('h3, .tile span');
      return {
        id: card.dataset.card,
        title: title ? title.textContent : '',
        column: columns.indexOf(Math.round(box.left)),
        top: box.top + root.parentElement.scrollTop,
        bottom: box.bottom + root.parentElement.scrollTop,
        height: box.height,
        width: box.width,
      };
    });
  });

const pinnedIds = async (page) => (await layout(page, 'pinned')).sort((a, b) => a.top - b.top || a.column - b.column).map((c) => c.id);

// The ids in reading order: row by row, left to right.
const reading = (cells, columns) => {
  const byColumn = Array.from({ length: columns }, (_, n) => cells.filter((c) => c.column === n).sort((a, b) => a.top - b.top));
  const out = [];
  for (let row = 0; byColumn.some((col) => col[row]); row += 1) {
    for (const col of byColumn) if (col[row]) out.push(col[row].id);
  }
  return out;
};

async function unpinAll(page) {
  const pinned = (await sync(page)).recipes.filter((r) => r.pinned !== undefined && r.pinned !== null);
  if (!pinned.length) return;
  const op = { id: `op_${Math.random().toString(36).slice(2, 14).padEnd(12, '0')}`, at: Date.now(), type: 'pins', ids: [] };
  const answer = await page.request.post('/api/ops', { data: { ops: [op] }, headers: sameOrigin(page) });
  expect(answer.ok()).toBe(true);
}

for (const [name, viewport, columns] of [
  ['an iPhone', null, 2],
  ['a wider screen', { width: 820, height: 1000 }, 3],
]) {
  test(`the grid is a masonry of ${columns} columns on ${name}: cards at their own height, read row by row`, async ({ page }) => {
    if (viewport) await page.setViewportSize(viewport);
    await openLibrary(page);
    await unpinAll(page);
    await page.reload();
    await expect(cards(page).first()).toBeVisible();
    await page.locator('select.sort').selectOption('title');
    const cells = await layout(page);
    const library = (await sync(page)).recipes.filter((r) => r.state === 'active');
    expect(cells).toHaveLength(library.length);
    expect(new Set(cells.map((c) => c.column))).toEqual(new Set(Array.from({ length: Math.min(columns, cells.length) }, (_, n) => n)));

    // Dealt across the columns in order: read row by row, the titles are sorted.
    const titles = new Map(library.map((r) => [r.id, r.doc.title]));
    const read = reading(cells, columns).map((id) => titles.get(id));
    const fold = (text) => text.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase();
    expect(read.map(fold)).toEqual([...read].map(fold).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)));

    // Each column packs its cards 22px apart whatever their heights, and no
    // card is stretched to a neighbour's height.
    for (let n = 0; n < columns; n += 1) {
      const column = cells.filter((c) => c.column === n).sort((a, b) => a.top - b.top);
      for (let i = 1; i < column.length; i += 1) {
        expect(Math.abs(column[i].top - column[i - 1].bottom - 22)).toBeLessThanOrEqual(0.6);
      }
    }
    const natural = await page.locator('[data-testid=grid] .card-wrap').evaluateAll((wraps) =>
      wraps.every((wrap) => Math.abs(wrap.getBoundingClientRect().height - wrap.firstElementChild.getBoundingClientRect().height) < 0.6),
    );
    expect(natural).toBe(true);
    const heights = new Set(cells.map((c) => Math.round(c.height)));
    if (cells.length > 6) {
      // Cards differ in height (one-line and two-line titles, tags), so the
      // columns do not line up in rows.
      expect(heights.size).toBeGreaterThan(1);
      const tops = (n) => cells.filter((c) => c.column === n).map((c) => Math.round(c.top));
      expect(tops(0).slice(1)).not.toEqual(tops(1).slice(1));
    }
    // Columns are equally wide and fill the row.
    const widths = new Set(cells.map((c) => Math.round(c.width)));
    expect(widths.size).toBe(1);
    // Covers still load lazily, a few at a time through the service worker.
    const lazy = await page.locator('[data-testid=grid] .cover img').evaluateAll((images) => images.every((image) => image.loading === 'lazy'));
    expect(lazy).toBe(true);
    await shot(page, `library-masonry-${columns}`);
    await page.locator('[data-testid=scroller]').evaluate((area) => area.scrollTo(0, area.scrollHeight));
    await shot(page, `library-masonry-${columns}-end`);
  });
}

test('the scroll position of the masonry is restored on the way back', async ({ page }) => {
  await openLibrary(page);
  const scroller = page.locator('[data-testid=scroller]');
  const max = await scroller.evaluate((area) => area.scrollHeight - area.clientHeight);
  test.skip(max < 300, 'the library is too short to scroll');
  await scroller.evaluate((area, top) => area.scrollTo(0, top), Math.min(max, 600));
  const before = await scroller.evaluate((area) => area.scrollTop);
  const target = await page.locator('[data-testid=grid] .card-wrap a.card').evaluateAll((links) => {
    const seen = links.find((link) => {
      const box = link.getBoundingClientRect();
      return box.top > 120 && box.bottom < window.innerHeight - 40;
    });
    return seen.getAttribute('href');
  });
  await page.locator(`[data-testid=grid] a.card[href="${target}"]`).click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
  await page.getByRole('button', { name: 'Back' }).click();
  await expect(cards(page).first()).toBeVisible();
  await expect.poll(() => scroller.evaluate((area) => area.scrollTop)).toBe(before);
});

test.describe('pinned recipes', () => {
  let first;
  let second;
  let third;

  test.afterAll(async ({ browser }) => {
    const page = await browser.newPage();
    await page.goto('/');
    await unpinAll(page);
    await page.close();
  });

  test('pin from the recipe menu and from a card menu; pinned recipes head the library in their order', async ({ page }) => {
    await openLibrary(page);
    await unpinAll(page);
    await page.reload();
    await expect(cards(page).first()).toBeVisible();
    await expect(page.locator('section.pinned')).toHaveCount(0);
    const order = reading(await layout(page), 2);
    [first, second, third] = order;
    const total = order.length;

    // From the recipe's own menu.
    await page.locator(`[data-testid=grid] .card-wrap[data-card="${first}"] a.card`).click();
    await expect(page.locator('main.recipe h1')).toBeVisible();
    await page.getByRole('button', { name: 'More', exact: true }).click();
    const pin = page.locator('.sheet.open [data-testid=pin]');
    await expect(pin).toHaveText('Pin');
    await pin.click();
    await expect(page.locator('[data-testid=toast]')).toContainText('Pinned');
    await page.getByRole('button', { name: 'More', exact: true }).click();
    await expect(page.locator('.sheet.open [data-testid=pin]')).toHaveText('Unpin');
    await page.locator('.sheet.open').getByRole('button', { name: 'Close' }).click();
    await page.getByRole('button', { name: 'Back' }).click();
    await expect(page.locator('section.pinned h2')).toHaveText('Pinned');
    expect(await pinnedIds(page)).toEqual([first]);
    // It is in the pinned section and not a second time in the grid.
    await expect(cards(page).and(page.locator(`[data-card="${first}"]`))).toHaveCount(0);
    expect(await cards(page).count()).toBe(total - 1);

    // From a card's menu in the library.
    await page.locator(`[data-testid=grid] .card-wrap[data-card="${second}"] [data-testid=card-more]`).click();
    const menu = page.locator('.sheet.open');
    await expect(menu.locator('[data-testid=card-pin]')).toHaveText('Pin');
    await expect(menu.locator('[data-testid=card-up]')).toHaveCount(0);
    await shot(page, 'pins-card-menu');
    await menu.locator('[data-testid=card-pin]').click();
    await expect(page.locator('.sheet.open')).toHaveCount(0);
    await expect.poll(() => pinnedIds(page)).toEqual([first, second]);
    // The menu button opens the menu and does not open the recipe.
    await expect(page.locator('main.library')).toBeVisible();
    await flushed(page);
    const stored = (await sync(page)).recipes.filter((r) => r.pinned !== undefined && r.pinned !== null);
    expect(stored.sort((a, b) => a.pinned - b.pinned).map((r) => r.id)).toEqual([first, second]);
    await shot(page, 'pins-section');

    // A search or a filter lists every match in one grid, pinned or not.
    await page.getByPlaceholder('Search recipes').fill('a');
    await expect(page.locator('section.pinned')).toHaveCount(0);
    await page.getByPlaceholder('Search recipes').fill('');
    await expect(page.locator('section.pinned')).toHaveCount(1);
    // The order survives a reload.
    await page.reload();
    await expect(cards(page, 'pinned').first()).toBeVisible();
    expect(await pinnedIds(page)).toEqual([first, second]);
  });

  test('a pinned card is reordered by pressing, holding and dragging it', async ({ page }) => {
    await openLibrary(page);
    await expect(cards(page, 'pinned')).toHaveCount(2);
    const box = async (id) => page.locator(`[data-testid=pinned] .card-wrap[data-pin="${id}"]`).boundingBox();
    const centre = async (id) => {
      const b = await box(id);
      return [b.x + b.width / 2, b.y + b.height / 2];
    };

    // A press that moves at once is a scroll, not a drag: nothing lifts.
    let [x, y] = await centre(first);
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x, y + 40, { steps: 4 });
    await expect(page.locator('[data-testid=drag-ghost]')).toHaveCount(0);
    await page.mouse.up();
    // That lift was a tap on the card's link.
    if (await page.locator('main.recipe').count()) await page.getByRole('button', { name: 'Back' }).click();
    await expect(cards(page, 'pinned')).toHaveCount(2);
    expect(await pinnedIds(page)).toEqual([first, second]);

    // Hold, then drag the first card onto the second's place.
    [x, y] = await centre(first);
    const [toX, toY] = await centre(second);
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.waitForTimeout(550);
    await expect(page.locator('[data-testid=drag-ghost]')).toBeVisible();
    await expect(page.locator(`[data-testid=pinned] .card-wrap[data-pin="${first}"]`)).toHaveClass(/lifted/);
    await page.mouse.move(toX, toY, { steps: 8 });
    // While held, nothing moves: the card under the finger is marked as the place.
    await expect(page.locator(`[data-testid=pinned] .card-wrap[data-pin="${second}"]`)).toHaveClass(/drop-here/);
    expect(await pinnedIds(page)).toEqual([first, second]);
    await shot(page, 'pins-dragging');
    await page.mouse.up();
    await expect(page.locator('[data-testid=drag-ghost]')).toHaveCount(0);
    // The lift does not open the recipe.
    await expect(page.locator('main.library')).toBeVisible();
    await expect(page.locator('main.recipe')).toHaveCount(0);
    expect(await pinnedIds(page)).toEqual([second, first]);
    await flushed(page);
    const stored = (await sync(page)).recipes.filter((r) => r.pinned !== undefined && r.pinned !== null);
    expect(stored.sort((a, b) => a.pinned - b.pinned).map((r) => r.id)).toEqual([second, first]);
    // Held and let go where it was, or outside the section, nothing changes.
    [x, y] = await centre(second);
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.waitForTimeout(550);
    await expect(page.locator('[data-testid=drag-ghost]')).toBeVisible();
    await page.mouse.move(x + 6, 40, { steps: 6 });
    await page.mouse.up();
    await expect(page.locator('[data-testid=drag-ghost]')).toHaveCount(0);
    await expect(page.locator('main.library')).toBeVisible();
    expect(await pinnedIds(page)).toEqual([second, first]);
    await page.waitForTimeout(450);
    // A plain tap still opens the recipe.
    await page.locator(`[data-testid=pinned] .card-wrap[data-pin="${second}"] a.card`).click();
    await expect(page.locator('main.recipe h1')).toBeVisible();
  });

  test('the card menu moves a pinned card up and down without a drag, and unpins', async ({ page }) => {
    await openLibrary(page);
    expect(await pinnedIds(page)).toEqual([second, first]);
    await page.locator(`[data-testid=pinned] .card-wrap[data-pin="${first}"] [data-testid=card-more]`).click();
    const menu = page.locator('.sheet.open');
    await expect(menu.locator('[data-testid=card-pin]')).toHaveText('Unpin');
    await expect(menu.locator('[data-testid=card-down]')).toBeDisabled();
    await expect(menu.locator('[data-testid=card-up]')).toContainText('2 of 2');
    await shot(page, 'pins-card-menu-pinned');
    await menu.locator('[data-testid=card-up]').click();
    // The grid follows under the open menu, and nothing asks for a reload.
    await expect.poll(() => pinnedIds(page)).toEqual([first, second]);
    await expect(menu).toBeVisible();
    await expect(page.locator('[data-testid=stale]')).toHaveCount(0);
    await expect(menu.locator('[data-testid=card-up]')).toBeDisabled();
    await menu.locator('[data-testid=card-down]').click();
    await expect.poll(() => pinnedIds(page)).toEqual([second, first]);
    await menu.locator('[data-testid=card-pin]').click();
    await expect.poll(() => pinnedIds(page)).toEqual([second]);
    await expect(cards(page).and(page.locator(`[data-card="${first}"]`))).toHaveCount(1);
    await flushed(page);
  });

  test('a list set elsewhere arrives by itself', async ({ page, request }) => {
    test.skip(!MOCK, 'another device is played by the mock server');
    await openLibrary(page);
    await request.post('/__mock/pins', { data: { ids: [third, first, second] } });
    await expect.poll(() => pinnedIds(page)).toEqual([third, first, second]);
    await request.post('/__mock/pins', { data: { ids: [] } });
    await expect(page.locator('section.pinned')).toHaveCount(0);
  });
});

// The grid the steppers move on, written out again here so the test does not
// borrow the app's own arithmetic.
const BANDS = {
  g: [[0, 1], [20, 5], [100, 10], [250, 25], [1000, 50]],
  ml: [[0, 5], [60, 25], [500, 50]],
  kg: [[0, 0.05], [1, 0.1], [3, 0.25]],
  l: [[0, 0.05], [1, 0.1], [3, 0.25]],
  tsp: [[0, 0.25], [2, 0.5]],
  cup: [[0, 0.25], [2, 0.5]],
  tbsp: [[0, 0.5], [4, 1]],
  oz: [[0, 0.5]],
  lb: [[0, 0.25]],
};
const COUNT = [[0, 0.5], [1, 1]];
function stepped(qty, unit, up) {
  const bands = BANDS[unit] || COUNT;
  const band = (at) => [...bands].reverse().find(([from]) => at >= from);
  const eps = 1e-6;
  if (up) {
    const [, size] = band(qty + eps);
    return Math.round((Math.floor((qty + eps) / size) * size + size) * 1e6) / 1e6;
  }
  const [from, size] = band(qty - eps);
  return Math.round(Math.max(Math.ceil((qty - eps) / size) * size - size, from, bands[0][1]) * 1e6) / 1e6;
}

test('the amount sheet steps each unit the way it is measured', async ({ page }) => {
  await openLibrary(page);
  const library = (await sync(page)).recipes.filter((r) => r.state === 'active');
  // One ingredient of each unit family the library has.
  const families = { weight: ['g', 'kg'], volume: ['ml', 'l'], spoons: ['tsp', 'tbsp'], cups: ['cup'], count: [undefined] };
  const found = {};
  for (const recipe of library) {
    for (const component of recipe.doc.components) {
      for (const ingredient of component.ingredients) {
        if (typeof ingredient.qty !== 'number') continue;
        for (const [family, units] of Object.entries(families)) {
          if (!found[family] && units.includes(ingredient.unit)) found[family] = { recipe, ingredient };
        }
      }
    }
  }
  expect(Object.keys(found).length).toBeGreaterThanOrEqual(3);
  for (const [family, { recipe, ingredient }] of Object.entries(found)) {
    await page.goto(`/r/${recipe.id}`);
    await expect(page.locator('main.recipe h1')).toBeVisible();
    await page.getByRole('tab', { name: 'Steps' }).click();
    await page.locator(`li[data-ingredient="${ingredient.id}"] [data-testid=edit-amount]`).click();
    const sheet = page.locator('.sheet.open');
    const field = sheet.locator('[data-testid=amount-qty]');
    let qty = ingredient.qty;
    await expect(field).toHaveValue(String(qty));
    for (const up of [true, true, false, false, false]) {
      await sheet.locator(up ? '[data-testid=amount-more]' : '[data-testid=amount-less]').click();
      qty = stepped(qty, ingredient.unit, up);
      await expect(field, `${family}: ${ingredient.qty} ${ingredient.unit || ''} ${up ? '+' : '-'}`).toHaveValue(String(qty));
    }
    if (family === 'weight') await shot(page, 'stepper-amount');
    // Nothing is saved.
    await sheet.getByRole('button', { name: 'Cancel' }).click();
    await expect(page.locator('.sheet.open')).toHaveCount(0);
  }
  expect((await sync(page)).recipes.find((r) => r.id === found.weight.recipe.id).revision).toBe(found.weight.recipe.revision);
});

test('the yield stepper moves a counted yield by one and a measured yield by its own steps', async ({ page }) => {
  await openLibrary(page);
  const library = (await sync(page)).recipes.filter((r) => r.state === 'active');
  const measured = library.find((r) => Object.keys(BANDS).includes(r.doc.yield.unit.replace(/s$/, '')));
  const counted = library.find((r) => !Object.keys(BANDS).includes(r.doc.yield.unit.replace(/s$/, '')) && r.doc.yield.qty >= 2);
  for (const recipe of [counted, measured].filter(Boolean)) {
    await page.goto(`/r/${recipe.id}`);
    await expect(page.locator('main.recipe h1')).toBeVisible();
    const unit = recipe.doc.yield.unit.replace(/s$/, '');
    await page.locator('.scale .seg').getByRole('button', { name: '1', exact: true }).click();
    let qty = recipe.doc.yield.qty;
    const shown = async () => Number((await page.locator('[data-testid=servings] b').innerText()).replace(',', '.').replace('½', '.5').replace('¼', '.25').replace('¾', '.75'));
    for (const up of [true, true, false, false, false]) {
      await page.getByRole('button', { name: up ? 'More servings' : 'Fewer servings' }).click();
      qty = stepped(qty, BANDS[unit] ? unit : undefined, up);
      if (!BANDS[unit]) qty = Math.max(qty, 1);
      expect(Math.abs((await shown()) - qty)).toBeLessThan(0.06);
    }
    await page.locator('.scale .seg').getByRole('button', { name: '1', exact: true }).click();
  }
});
