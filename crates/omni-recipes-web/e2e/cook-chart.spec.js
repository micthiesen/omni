// Cook mode's chart (docs/fond/cook-chart-steps.md): a tap on a step cell
// opens its details in a sheet, a press and hold marks it done, an
// ingredient is crossed off by a tap, and a running timer counts down in its
// cell.
//
// Runs on the mock server and, with FOND_BASE_URL, on a real Omni. Every
// session a test starts is discarded, which logs nothing. The tests skip
// when the recipe already has an open session, which is not theirs to
// discard.
//
//   FOND_RECIPE_COOK    the recipe to cook (default 'Chicken Piccata')
//   FOND_RECIPE_CHART   a recipe with a wide chart (default 'Lasagne alla Bolognese')
//   FOND_SHOTS          a directory for light and dark screenshots
//
// What Playwright's WebKit cannot play is a finger: the holds here are made
// with the mouse and with pointer events of type "touch" sent to the cell.
// The iOS callout, text selection and the pointercancel a real scroll sends
// are only on an iPhone.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const RECIPES = {
  cook: process.env.FOND_RECIPE_COOK || 'Chicken Piccata',
  wide: process.env.FOND_RECIPE_CHART || 'Lasagne alla Bolognese',
};
const SHOTS = process.env.FOND_SHOTS;
const COUNTDOWN = /^\d+:\d\d(:\d\d)?$/;
const toast = (page) => page.locator('[data-testid=toast]');
// Longer than the app's 400 ms.
const HOLD_MS = 700;

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

// Omni refuses writes that do not come from the app's own origin.
const sameOrigin = (page) => ({ origin: new URL(page.url()).origin });

async function flushed(page) {
  await expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
}

// The recipe's steps in written order, as the server has them.
async function stepsOf(page, title) {
  const sync = await (await page.request.get('/api/sync', { headers: sameOrigin(page) })).json();
  const recipe = sync.recipes.find((found) => found.doc.title === title && found.state === 'active');
  return recipe.doc.components.flatMap((component) => component.steps);
}

// What the server has checked in a session: the event log folded.
async function checkedOnServer(page, sessionId) {
  const session = await (await page.request.get(`/api/cook/${sessionId}`, { headers: sameOrigin(page) })).json();
  const checked = new Set();
  for (const event of session.events) {
    if (event.type === 'check') checked.add(event.target);
    if (event.type === 'uncheck') checked.delete(event.target);
  }
  return [...checked];
}

// Starts a cook of `title` and opens its chart. The test is skipped when
// someone else's session is open.
async function cookChart(page, title = RECIPES.cook) {
  await page.goto('/');
  await expect(page.locator('[data-testid=grid] a.card').first()).toBeVisible();
  await page.getByPlaceholder('Search recipes').fill(title);
  await card(page, title).first().click();
  await expect(page.locator('main.recipe h1')).toHaveText(title);
  const cook = page.locator('[data-testid=cook]');
  test.skip((await cook.innerText()).trim() !== 'Cook', 'this recipe has an open cook session that is not ours');
  await cook.click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  await page.getByRole('tab', { name: 'Chart' }).click();
  const chart = page.locator('[data-testid=chart]');
  await expect(chart.locator('table.chart').first()).toBeVisible();
  return { chart, sessionId: page.url().split('/').pop() };
}

async function discardCook(page) {
  const open = page.locator('.sheet.open');
  if (await open.count()) await open.getByRole('button', { name: 'Close' }).click();
  await page.locator('[data-testid=leave]').click();
  await page.locator('[data-testid=leave-discard]').click();
  await page.locator('[data-testid=leave-discard-confirm]').click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
  await flushed(page);
}

// The labelled cell of a step (a set-up step that stretches right is two
// table cells; the label is in the first).
const stepCell = (chart, id) => chart.locator(`td.step[data-key="${id}"], td.setup[data-key="${id}"]:not(.fill)`);

// The first step that is ready and passes `wanted`, with its number.
async function readyStep(page, chart, title, wanted = () => true) {
  const steps = await stepsOf(page, title);
  const ready = await chart
    .locator('td.step.ready, td.setup.ready:not(.fill)')
    .evaluateAll((cells) => cells.map((cell) => cell.dataset.key));
  const id = ready.find((key) => wanted(steps.find((step) => step.id === key)));
  if (!id) return null;
  const step = steps.find((found) => found.id === id);
  return { id, step, number: steps.indexOf(step) + 1, total: steps.length, cell: stepCell(chart, id) };
}

// A test that fails in cook mode must not leave its session open: the next
// one would find the recipe taken.
test.afterEach(async ({ page }, info) => {
  if (info.status === info.expectedStatus || !/\/cook\//.test(page.url())) return;
  await discardCook(page).catch(() => {});
});

const stepSheet = (page) => page.locator('.sheet.open [data-testid=step-sheet]');

async function closeSheet(page) {
  await page.locator('.sheet.open').getByRole('button', { name: 'Close' }).click();
  await expect(page.locator('.sheet.open')).toHaveCount(0);
}

// A press with the mouse in the middle of `cell`, held for `ms`.
async function hold(page, cell, ms = HOLD_MS) {
  await cell.scrollIntoViewIfNeeded();
  const box = await cell.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(ms);
  await page.mouse.up();
}

// A finger on `cell`, as the events a touch sends, in the order WebKit sends
// them. `then` names what happens while it is down: 'move' (14px sideways),
// 'cancel' (the browser took the touch for a scroll), 'scroll' (the page
// moved under it) or nothing. A touch leaves the cell as it lifts and its
// click comes last; a touch that moved or scrolled never clicks.
const touch = (cell, then, ms = HOLD_MS) =>
  cell.evaluate(
    async (el, [what, wait]) => {
      const box = el.getBoundingClientRect();
      const at = { clientX: box.left + box.width / 2, clientY: box.top + box.height / 2 };
      const send = (type, more = {}) =>
        el.dispatchEvent(
          new PointerEvent(type, {
            bubbles: !/enter|leave/.test(type),
            isPrimary: true,
            pointerId: 9,
            pointerType: 'touch',
            button: 0,
            ...at,
            ...more,
          }),
        );
      send('pointerdown');
      if (what === 'move') send('pointermove', { clientX: at.clientX + 14 });
      if (what === 'cancel') send('pointercancel');
      if (what === 'scroll') document.querySelector('.scroller').dispatchEvent(new Event('scroll'));
      await new Promise((done) => setTimeout(done, wait));
      if (what !== 'cancel') send('pointerup');
      send('pointerout');
      send('pointerleave');
      if (!what) el.click();
    },
    [then, ms],
  );

// What the chart's layout must keep whatever a cell shows: whole-pixel
// columns and rows, and every label and countdown inside its cell.
const layout = (chart) =>
  chart.evaluate((el) => {
    const problems = [];
    const whole = (value) => Math.abs(value - Math.round(value)) < 0.01;
    for (const row of el.querySelectorAll('table.chart tr')) {
      const height = row.getBoundingClientRect().height;
      if (!whole(height)) problems.push(`a row ${height}px high`);
    }
    for (const cell of el.querySelectorAll('td.step, td.setup:not(.fill)')) {
      const box = cell.getBoundingClientRect();
      for (const part of cell.querySelectorAll(':scope > span')) {
        const rect = part.getBoundingClientRect();
        const inside =
          rect.left >= box.left - 0.5 && rect.right <= box.right + 0.5 && rect.top >= box.top - 0.5 && rect.bottom <= box.bottom + 0.5;
        if (!inside) problems.push(`"${part.textContent}" does not fit the cell "${cell.textContent}"`);
      }
      if (cell.scrollWidth > cell.clientWidth) problems.push(`"${cell.textContent}" overflows sideways`);
      const digits = cell.querySelector('.cell-timer b');
      if (digits && digits.getBoundingClientRect().height > 18.5) problems.push(`the countdown "${digits.textContent}" breaks`);
      for (const element of cell.querySelectorAll('*')) {
        const style = getComputedStyle(element);
        if (style.writingMode !== 'horizontal-tb' || style.transform !== 'none') problems.push(`turned text in "${cell.textContent}"`);
      }
    }
    const columns = [...el.querySelectorAll('table.chart')].map((table) =>
      [...table.querySelectorAll('col')].map((col) => col.style.width),
    );
    return { problems, columns };
  });

test('a tap on a step cell opens its details, and Mark done there marks it', async ({ page }) => {
  const { chart, sessionId } = await cookChart(page);
  await shot(page, 'cook-chart-cell');
  await expect(chart.locator('.hint')).toHaveText(
    'Tap a step for details. Press and hold it until it fills to mark it done, and again to undo. Tap an ingredient to cross it off.',
  );
  const found = await readyStep(page, chart, RECIPES.cook, (step) => step.inputs.length > 0);
  test.skip(!found, 'no ready step with inputs');
  const { id, step, number, total, cell } = found;

  // The cell is a button with a name, for a keyboard or a screen reader.
  await expect(cell).toHaveAttribute('role', 'button');
  await expect(cell).toHaveAttribute('tabindex', '0');
  expect((await cell.innerText()).trim().length).toBeGreaterThan(0);

  await cell.click();
  const sheet = stepSheet(page);
  await expect(sheet).toHaveAttribute('data-step', id);
  await expect(page.locator('.sheet.open')).toHaveCount(1);
  await expect(sheet.locator('.step-no')).toHaveText(new RegExp(`^Step ${number} of ${total}`));
  expect((await sheet.locator('[data-testid=step-text]').innerText()).trim().length).toBeGreaterThan(10);
  const chips = sheet.locator('.chip.input');
  await expect(chips).toHaveCount(step.inputs.length);
  // The chips carry the amounts: at least one has a number in it.
  expect((await chips.allInnerTexts()).some((text) => /\d|½|¼|¾|⅓|⅔/.test(text))).toBe(true);
  if (step.time) await expect(sheet.locator('[data-testid=timer]')).toHaveText(/^Start .+ timer$/);
  await expect(sheet.locator('[data-testid=recipe-notes]')).toBeVisible();

  // The button is large and wholly on screen.
  const done = sheet.locator('[data-testid=sheet-done]');
  await expect(done).toHaveText('Mark done');
  const box = await done.boundingBox();
  expect(box.height).toBeGreaterThanOrEqual(56);
  expect(box.width).toBeGreaterThan(300);
  expect(box.y + box.height).toBeLessThanOrEqual(page.viewportSize().height);
  await shot(page, 'cook-chart-sheet');

  // Mark done closes the sheet; the cell is dimmed and the server has it.
  await done.click();
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await expect(cell).toHaveClass(/done/);
  await flushed(page);
  expect(await checkedOnServer(page, sessionId)).toContain(id);
  await cell.scrollIntoViewIfNeeded();
  await shot(page, 'cook-chart-done');

  // Opened again it says so and offers Undo, which leaves the step in front.
  await cell.click();
  await expect(sheet.locator('.step-no')).toContainText('done');
  await expect(sheet.locator('[data-testid=sheet-done]')).toHaveCount(0);
  await expect(sheet.locator('[data-testid=sheet-undo]')).toHaveText('Undo');
  await shot(page, 'cook-chart-sheet-done');
  await sheet.locator('[data-testid=sheet-undo]').click();
  await expect(sheet.locator('[data-testid=sheet-done]')).toBeVisible();
  await expect(cell).not.toHaveClass(/done/);
  await flushed(page);
  expect(await checkedOnServer(page, sessionId)).not.toContain(id);

  // Enter on the cell opens the same sheet.
  await closeSheet(page);
  await cell.focus();
  await page.keyboard.press('Enter');
  await expect(sheet).toHaveAttribute('data-step', id);

  await discardCook(page);
});

test('press and hold marks a step done without the sheet; a scroll is not a hold', async ({ page }) => {
  const { chart, sessionId } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook);
  test.skip(!found, 'no ready step');
  const { id, cell } = found;

  // A long press selects nothing. (The iOS callout is switched off beside
  // it in the stylesheet; this WebKit build does not know the property.)
  expect(await cell.evaluate((el) => getComputedStyle(el).webkitUserSelect || getComputedStyle(el).userSelect)).toBe('none');
  // Nor does anything around the chart: iOS would select the nearest text
  // that can be selected instead.
  const selectable = await page.evaluate(() =>
    [...document.querySelectorAll('.page.cook *')]
      .filter((el) => [...el.childNodes].some((node) => node.nodeType === 3 && node.textContent.trim()))
      .filter((el) => (getComputedStyle(el).webkitUserSelect || getComputedStyle(el).userSelect) !== 'none')
      .map((el) => el.className || el.tagName),
  );
  expect(selectable).toEqual([]);
  expect(await cell.evaluate((el) => getComputedStyle(el).touchAction)).toBe('manipulation');
  // A selection or a menu that starts on the chart is refused.
  expect(
    await cell.evaluate((el) =>
      ['selectstart', 'contextmenu'].map((type) => el.dispatchEvent(new Event(type, { bubbles: true, cancelable: true }))),
    ),
  ).toEqual([false, false]);

  // While the press lasts the cell fills; lifted early, it is a tap and the
  // fill is gone.
  const box = await cell.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await expect(cell).toHaveClass(/pressing/);
  // (This WebKit build does not advance an animation while a test holds
  // the mouse down, so the fill is checked by its rule, not its progress.)
  expect(await cell.evaluate((el) => [getComputedStyle(el).animationName, getComputedStyle(el).backgroundRepeat])).toEqual([
    'cook-press',
    'no-repeat',
  ]);
  // Past HOLD_MS (400 ms): without screenshots nothing else fills the time.
  await page.waitForTimeout(450);
  await shot(page, 'cook-chart-pressing');
  await page.mouse.up();
  await expect(cell).not.toHaveClass(/pressing/);
  await expect(cell).toHaveClass(/done/);
  // The hold says what it did and takes it back.
  await expect(toast(page)).toContainText('Done: ');
  await shot(page, 'cook-chart-held');
  await toast(page).getByRole('button', { name: 'Undo' }).click();
  await expect(cell).not.toHaveClass(/done/);
  await expect(toast(page)).toHaveCount(0);

  await hold(page, cell);
  await expect(cell).toHaveClass(/done/);
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  expect(await page.evaluate(() => String(getSelection()))).toBe('');
  await flushed(page);
  expect(await checkedOnServer(page, sessionId)).toContain(id);

  // A short tap afterwards is still a tap.
  await page.waitForTimeout(500);
  await cell.click();
  await expect(stepSheet(page).locator('[data-testid=sheet-undo]')).toBeVisible();
  await closeSheet(page);

  // Held again, it is un-marked. A finger does the same as the mouse.
  await hold(page, cell);
  await expect(cell).not.toHaveClass(/done/);
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await touch(cell, null);
  await expect(cell).toHaveClass(/done/);
  await touch(cell, null);
  await expect(cell).not.toHaveClass(/done/);

  // A press shorter than the hold is a tap.
  await page.waitForTimeout(500);
  await touch(cell, null, 200);
  await expect(stepSheet(page).locator('[data-testid=sheet-done]')).toBeVisible();
  await closeSheet(page);
  await expect(cell).not.toHaveClass(/done/);
  // One that moves, one the browser takes for a scroll and one the page
  // scrolls under do nothing.
  await touch(cell, 'move');
  await touch(cell, 'cancel');
  await touch(cell, 'scroll');
  await page.waitForTimeout(200);
  await expect(cell).not.toHaveClass(/done/);
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await flushed(page);
  expect(await checkedOnServer(page, sessionId)).not.toContain(id);

  // Leaving the chart with a press still down marks nothing.
  await cell.evaluate((el) => {
    const box = el.getBoundingClientRect();
    el.dispatchEvent(
      new PointerEvent('pointerdown', { bubbles: true, isPrimary: true, pointerId: 9, pointerType: 'touch', clientX: box.left + 5, clientY: box.top + 5 }),
    );
  });
  // At once: Playwright's own click takes longer than the hold.
  await page.getByRole('tab', { name: 'Steps' }).evaluate((el) => el.click());
  await page.waitForTimeout(HOLD_MS);
  await page.getByRole('tab', { name: 'Chart' }).click();
  await expect(stepCell(chart, id)).not.toHaveClass(/done/);

  await discardCook(page);
});

test('a tap on an ingredient cell still crosses it off', async ({ page }) => {
  const { chart, sessionId } = await cookChart(page);
  const leaf = chart.locator('td.lead.leaf.tap').first();
  const key = await leaf.getAttribute('data-key');
  await expect(leaf).toHaveAttribute('aria-pressed', 'false');
  await leaf.click();
  await expect(leaf).toHaveClass(/done/);
  await expect(leaf).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await flushed(page);
  expect(await checkedOnServer(page, sessionId)).toContain(key);
  // A hold on an ingredient is only a slow tap.
  await hold(page, leaf);
  await expect(leaf).not.toHaveClass(/done/);
  await expect(page.locator('.sheet.open')).toHaveCount(0);
  await discardCook(page);
});

test('a timer started in the sheet counts down in the sheet, the cell and the strip', async ({ page }) => {
  const { chart } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook, (step) => !!step.time);
  test.skip(!found, 'no ready step with a time');
  const { cell } = found;
  const before = await layout(chart);
  expect(before.problems).toEqual([]);

  await cell.click();
  const sheet = stepSheet(page);
  await sheet.locator('[data-testid=timer]').click();
  const inSheet = sheet.locator('[data-testid=countdown]');
  await expect(inSheet).toHaveText(COUNTDOWN);
  const inCell = cell.locator('[data-testid=cell-timer]');
  await expect(inCell).toHaveText(COUNTDOWN);
  const strip = page.locator('[data-testid=running]');
  await expect(strip.locator('b')).toHaveText(COUNTDOWN);
  // All three follow the same clock.
  const first = await inCell.innerText();
  await expect(inCell).not.toHaveText(first);
  await expect.poll(async () => (await inSheet.innerText()) === (await inCell.innerText())).toBe(true);
  await shot(page, 'cook-chart-sheet-timer');

  // The countdown costs no width, and rows stay whole pixels.
  const during = await layout(chart);
  expect(during.problems).toEqual([]);
  expect(during.columns).toEqual(before.columns);

  await closeSheet(page);
  await page.locator('[data-testid=scroller]').evaluate((el) => el.scrollTo(0, 0));
  await shot(page, 'cook-chart-timer');

  // The dock is under the screen: the chart did not move when it came, and
  // a tap on a timer opens its step over the chart.
  expect(await strip.evaluate((el) => el.closest('.scroller'))).toBeNull();
  const digits = await strip.locator('b').boundingBox();
  await strip.click();
  await expect(sheet).toBeVisible();
  await expect(page.getByRole('tab', { name: 'Chart' })).toHaveAttribute('aria-selected', 'true');
  await closeSheet(page);
  // The digits keep their place as they change.
  await expect(strip.locator('b')).not.toHaveText(await strip.locator('b').innerText());
  expect(await strip.locator('b').boundingBox()).toEqual(digits);

  // A step marked done stops its timer; the toast's Undo brings both back.
  await hold(page, cell);
  await expect(cell).toHaveClass(/done/);
  await expect(inCell).toHaveCount(0);
  await expect(strip).toHaveCount(0);
  await toast(page).getByRole('button', { name: 'Undo' }).click();
  await expect(cell).not.toHaveClass(/done/);
  await expect(inCell).toHaveText(COUNTDOWN);
  await expect(strip).toHaveCount(1);

  // Cancel removes all three, and can be taken back.
  await page.waitForTimeout(500);
  await cell.click();
  await sheet.getByRole('button', { name: 'Cancel' }).click();
  await expect(inSheet).toHaveCount(0);
  await expect(toast(page)).toContainText('Timer cancelled');
  await toast(page).getByRole('button', { name: 'Undo' }).click();
  await expect(inSheet).toHaveText(COUNTDOWN);
  await sheet.getByRole('button', { name: 'Cancel' }).click();
  await expect(sheet.locator('[data-testid=timer]')).toBeVisible();
  await expect(inSheet).toHaveCount(0);
  await expect(inCell).toHaveCount(0);
  await expect(strip).toHaveCount(0);
  expect((await layout(chart)).columns).toEqual(before.columns);

  await discardCook(page);
});

test('a timer that ran out counts up, in the alert colour, until it is stopped', async ({ page }) => {
  const { chart, sessionId } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook, (step) => !!step.time);
  test.skip(!found, 'no ready step with a time');
  const { id, cell } = found;
  // Two timers, one about to run out: set as another device would.
  const other = (await stepsOf(page, RECIPES.cook)).find((step) => step.id !== id);
  const event = (step, endsAt) => ({ seq: 0, clientEventId: `ev_e2e${step}${Date.now()}`, at: Date.now(), actor: 'app', type: 'timer', step, endsAt });
  const pushed = await page.request.post(`/api/cook/${sessionId}/events`, {
    headers: sameOrigin(page),
    data: { events: [event(id, Date.now() + 2500), event(other.id, Date.now() + 3_600_000)] },
  });
  expect(pushed.ok()).toBe(true);
  const pills = page.locator('[data-testid=running]');
  await expect(pills).toHaveCount(2);
  // Two to a row, inside the screen.
  const [a, b] = [await pills.nth(0).boundingBox(), await pills.nth(1).boundingBox()];
  expect(a.y).toBe(b.y);
  expect(Math.max(a.x + a.width, b.x + b.width)).toBeLessThanOrEqual(390);
  const late = page.locator(`[data-testid=running][data-step="${id}"]`);
  await expect(late).toHaveClass(/late/, { timeout: 8000 });
  await expect(late.locator('b')).toHaveText(/^\+\d+:\d\d$/);
  await expect(cell.locator('[data-testid=cell-timer]')).toHaveClass(/late/);
  await expect(page.locator('[data-testid=timer-alert]')).toContainText('Timer done');
  // The alert is above the dock, not on it.
  const alert = await page.locator('[data-testid=timer-alert]').boundingBox();
  const dock = await page.locator('[data-testid=timer-dock]').boundingBox();
  expect(alert.y + alert.height).toBeLessThanOrEqual(dock.y);
  await shot(page, 'cook-chart-timer-late');
  await late.click();
  await stepSheet(page).getByRole('button', { name: 'Stop' }).click();
  await expect(late).toHaveCount(0);
  await expect(page.locator('[data-testid=timer-alert]')).toHaveCount(0);
  // Stopping one that ran out needs no undo.
  await expect(toast(page)).toHaveCount(0);
  await expect(pills).toHaveCount(1);
  await discardCook(page);
});

test('the sheet leads to the recipe notes and to the amounts', async ({ page }) => {
  const { chart } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook, (step) => step.inputs.length > 0);
  test.skip(!found, 'no ready step with inputs');
  const { cell } = found;

  await cell.click();
  await stepSheet(page).locator('[data-testid=recipe-notes]').click();
  // One sheet at a time: the step sheet made way.
  await expect(page.locator('.sheet.open')).toHaveCount(1);
  await expect(page.locator('.sheet.open h2')).toHaveText('Recipe notes');
  await expect(page.locator('.sheet.open').getByLabel('Notes')).toBeVisible();
  await expect(stepSheet(page)).toHaveCount(0);
  await closeSheet(page);

  await cell.click();
  const amounts = stepSheet(page).locator('[data-testid=step-amounts]');
  if (await amounts.count()) {
    await amounts.click();
    await expect(page.locator('.sheet.open')).toHaveCount(1);
    // The chooser when the step has several ingredients, else the amount
    // sheet itself. Nothing is saved.
    await expect(page.locator('.sheet.open h2')).toHaveText(/^Change an amount/i);
    await expect(stepSheet(page)).toHaveCount(0);
    const choice = page.locator('.sheet.open [data-testid=change-amount]').first();
    if (await choice.count()) {
      await choice.click();
      await expect(page.locator('.sheet.open')).toHaveCount(1);
      await expect(page.locator('.sheet.open h2')).toHaveText(/^Change amount: /);
    }
    await closeSheet(page);
  } else {
    await closeSheet(page);
  }
  await discardCook(page);
});

test('an input crossed off in the sheet is crossed off in the chart', async ({ page }) => {
  const { chart } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook, (step) => step.inputs.length > 0);
  test.skip(!found, 'no ready step with inputs');
  await found.cell.click();
  const chip = stepSheet(page).locator('.chip.input').first();
  const key = await chip.getAttribute('data-key');
  const leaf = chart.locator(`td.lead[data-key="${key}"]`);
  await expect(leaf).not.toHaveClass(/done/);
  await chip.click();
  await expect(chip).toHaveAttribute('aria-pressed', 'true');
  await expect(leaf).toHaveClass(/done/);
  await expect(stepSheet(page)).toBeVisible();
  await chip.click();
  await expect(chip).toHaveAttribute('aria-pressed', 'false');
  await expect(leaf).not.toHaveClass(/done/);
  await discardCook(page);
});

test('the step sheet keeps to the shell: nothing behind it scrolls, and it keeps its place', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 420 });
  const { chart } = await cookChart(page);
  const found = await readyStep(page, chart, RECIPES.cook, (step) => step.inputs.length > 1 && !!step.time);
  test.skip(!found, 'no ready step with inputs and a time');
  const area = () =>
    page.evaluate(() => {
      const el = document.querySelector('.scroller');
      return { top: el.scrollTop, doc: window.scrollY, scrollers: document.querySelectorAll('.scroller').length };
    });
  // Scrolls what lies under the point by `dy`, the way a drag there would.
  const drag = (x, y, dy) =>
    page.evaluate(
      ([px, py, by]) => {
        let el = document.elementFromPoint(px, py);
        while (el && !(/(auto|scroll)/.test(getComputedStyle(el).overflowY) && el.scrollHeight > el.clientHeight)) {
          el = el.parentElement;
        }
        (el || document.scrollingElement).scrollBy(0, by);
      },
      [x, y, dy],
    );
  await found.cell.click();
  await expect(stepSheet(page)).toBeVisible();
  const before = await area();
  expect(before.scrollers).toBe(1);
  const sheet = page.locator('.sheet.open');
  expect(await sheet.evaluate((el) => [getComputedStyle(el).position, getComputedStyle(el).overscrollBehaviorY])).toEqual([
    'fixed',
    'contain',
  ]);
  const rect = await sheet.boundingBox();
  // On this short screen the sheet scrolls; a pull on it, or on the scrim
  // above it, moves nothing behind.
  await drag(rect.x + rect.width / 2, rect.y + rect.height / 2, 100000);
  const scrolled = await sheet.evaluate((el) => el.scrollTop);
  expect(scrolled).toBeGreaterThan(0);
  await drag(rect.x + rect.width / 2, Math.max(2, rect.y / 2), 100000);
  expect(await area()).toEqual(before);

  // Live state does not move the reader: a chip crossed off, a timer
  // started and two seconds of countdown leave the sheet where it was.
  const place = () => sheet.evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop);
  expect(await place()).toBeLessThanOrEqual(1);
  const chip = stepSheet(page).locator('.chip.input').first();
  await chip.evaluate((el) => el.click());
  await expect(chip).toHaveAttribute('aria-pressed', 'true');
  expect(await sheet.evaluate((el) => el.scrollTop)).toBe(scrolled);
  await stepSheet(page).locator('[data-testid=timer]').evaluate((el) => el.click());
  const countdown = stepSheet(page).locator('[data-testid=countdown]');
  await expect(countdown).toHaveText(COUNTDOWN);
  const shown = await countdown.innerText();
  await expect(countdown).not.toHaveText(shown);
  await page.waitForTimeout(1200);
  // The running timer is as tall as its button, so the bottom stays the bottom.
  expect(await place()).toBeLessThanOrEqual(1);
  expect(await sheet.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  expect(await area()).toEqual(before);

  await discardCook(page);
});

test('a wide chart keeps its columns and whole-pixel rows with timers running', async ({ page }) => {
  const { chart } = await cookChart(page, RECIPES.wide);
  const before = await layout(chart);
  expect(before.problems).toEqual([]);
  // Every step that has a time gets a timer, set-up steps and the hours-long
  // ones in the narrowest columns included.
  const steps = (await stepsOf(page, RECIPES.wide)).filter((step) => step.time);
  test.skip(steps.length === 0, 'no step of this recipe has a time');
  for (const step of steps) {
    const cell = stepCell(chart, step.id);
    await cell.click();
    await stepSheet(page).locator('[data-testid=timer]').click();
    await expect(stepSheet(page).locator('[data-testid=countdown]')).toHaveText(COUNTDOWN);
    await closeSheet(page);
    await expect(cell.locator('[data-testid=cell-timer]')).toHaveText(COUNTDOWN);
  }
  const during = await layout(chart);
  expect(during.problems).toEqual([]);
  expect(during.columns).toEqual(before.columns);
  await expect(page.locator('[data-testid=running]')).toHaveCount(steps.length);
  if (SHOTS) {
        for (const [index, block] of (await chart.locator('.chart-block').all()).entries()) {
      await block.scrollIntoViewIfNeeded();
      await shot(page, `cook-chart-wide-timers-${index + 1}`);
    }
    const last = chart.locator('.chart-block .chart-scroll').last();
    await last.evaluate((el) => {
      el.scrollLeft = el.scrollWidth;
    });
    await shot(page, 'cook-chart-wide-timers-end');
  }
  await discardCook(page);
});
