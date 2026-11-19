// The chart's lines and its pinned ingredient column (docs/fond/chart-borders.md),
// in every recipe of the library, in the recipe view and in cook mode.
//
//   FOND_CHART_SHOTS   a directory for one screenshot per chart block and
//                      sideways scroll position (start, 37px, middle, end)
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const SHOTS = process.env.FOND_CHART_SHOTS;

const slug = (title) =>
  title
    .normalize('NFD')
    .replace(/[^a-zA-Z0-9]+/g, '-')
    .replace(/^-|-$/g, '')
    .toLowerCase();

const activeRecipes = async (page) =>
  (await (await page.request.get('/api/sync')).json()).recipes.filter((recipe) => recipe.state === 'active');

async function openChart(page, recipe) {
  await page.goto(`/r/${recipe.id}`);
  await expect(page.locator('main.recipe h1')).toHaveText(recipe.doc.title);
  await page.locator('.scale .seg button', { hasText: /^1$/ }).click();
  await page.getByRole('tab', { name: 'Chart' }).click();
  await expect(page.locator('[data-testid=chart] table.chart').first()).toBeVisible();
}

// Starts a cook from the recipe page and opens its chart; false when someone
// else's session is open, which is not ours to discard.
async function openCookChart(page) {
  const cook = page.locator('[data-testid=cook]');
  if ((await cook.innerText()).trim() !== 'Cook') return false;
  await cook.click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  await page.getByRole('tab', { name: 'Chart' }).click();
  await expect(page.locator('[data-testid=chart] table.chart').first()).toBeVisible();
  return true;
}

async function discardCook(page) {
  await page.locator('[data-testid=leave]').click();
  await page.locator('[data-testid=leave-discard]').click();
  await page.locator('[data-testid=leave-discard-confirm]').click();
  await expect(page.locator('main.recipe h1')).toBeVisible();
  await expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
}

// The sideways positions worth looking at: the ends, the middle, and a few
// pixels in, where the pinned column cuts through a step cell.
const positions = (max) =>
  max <= 1
    ? [['start', 0]]
    : [
        ['start', 0],
        ['early', Math.min(37, max)],
        ['middle', Math.round(max / 2)],
        ['end', max],
      ];

async function scrollBlock(block, left) {
  await block.locator('.chart-scroll').evaluate((scroller, to) => {
    scroller.scrollLeft = to;
    return new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
  }, left);
}

const maxScroll = (block) =>
  block.locator('.chart-scroll').evaluate((scroller) => scroller.scrollWidth - scroller.clientWidth);

async function captureBlocks(page, name) {
  const blocks = page.locator('[data-testid=chart] .chart-block');
  for (let index = 0; index < (await blocks.count()); index += 1) {
    const block = blocks.nth(index);
    for (const [label, left] of positions(await maxScroll(block))) {
      await scrollBlock(block, left);
      await block.scrollIntoViewIfNeeded();
      await block.screenshot({ path: path.join(SHOTS, `${name}-block${index + 1}-${label}.png`) });
    }
    await scrollBlock(block, 0);
  }
}

// What is wrong with the lines of the chart blocks on screen at their current
// scroll positions, as a list of sentences (empty when every line follows
// docs/fond/chart-borders.md).
const lineProblems = (page) =>
  page.locator('[data-testid=chart]').evaluate((chart) => {
    const problems = [];
    const whole = (value) => Math.abs(value - Math.round(value)) < 0.01;
    const px = (value) => Math.round(parseFloat(value) * 100) / 100;
    // A colour the browser has resolved, to compare computed borders with.
    const probe = document.createElement('i');
    chart.append(probe);
    const resolved = (token) => {
      probe.style.color = `var(${token})`;
      return getComputedStyle(probe).color;
    };
    const [strong, light] = [resolved('--line-strong'), resolved('--line')];
    probe.remove();
    const stepOf = (cell) =>
      cell && (cell.classList.contains('leaf') || cell.classList.contains('ref')) ? cell.dataset.key.split('/')[0] : null;

    chart.querySelectorAll('.chart-block').forEach((block, index) => {
      const say = (text) => problems.push(`block ${index + 1}: ${text}`);
      const scroller = block.querySelector('.chart-scroll');
      const table = block.querySelector('table.chart');
      const frame = table.getBoundingClientRect();
      const port = scroller.getBoundingClientRect();
      const rows = [...table.querySelectorAll('tr')];
      const leads = rows.map((row) => row.querySelector('td.lead'));

      for (const col of table.querySelectorAll('col')) {
        if (!/^\d+px$/.test(col.style.width)) say(`a column of ${col.style.width}`);
      }
      rows.forEach((row, at) => {
        if (!whole(row.getBoundingClientRect().height)) say(`row ${at + 1} is ${row.getBoundingClientRect().height}px high`);
        if (!leads[at]) say(`row ${at + 1} has no ingredient cell`);
      });

      for (const cell of table.querySelectorAll('td')) {
        const style = getComputedStyle(cell);
        const name = cell.textContent.trim() || `the stretch after "${cell.previousElementSibling?.textContent.trim()}"`;
        const lead = cell.classList.contains('lead');
        const split = lead && cell.nextElementSibling?.classList.contains('fill');
        const first = cell.parentElement === rows[0];
        const widths = ['Top', 'Right', 'Bottom', 'Left'].map((side) => px(style[`border${side}Width`]));
        const want = [first ? 1 : 0, split ? 0 : 1, 1, lead ? 1 : 0];
        if (widths.join() !== want.join()) say(`"${name}" has borders ${widths} (top, right, bottom, left), not ${want}`);

        // Light only between two inputs of the same step; everything else
        // that is drawn is the strong line.
        const row = rows.indexOf(cell.parentElement);
        const input = lead || cell.classList.contains('fill');
        const step = stepOf(leads[row]);
        const sameStep = input && step !== null && step === stepOf(leads[row + cell.rowSpan]);
        if (style.borderBottomColor !== (sameStep ? light : strong)) {
          say(`"${name}" has a ${sameStep ? 'strong' : 'light'} line under it`);
        }
        if (!split && style.borderRightColor !== strong) say(`"${name}" has a right line of ${style.borderRightColor}`);
        if (first && style.borderTopColor !== strong) say(`"${name}" has a top line of ${style.borderTopColor}`);

        const box = cell.getBoundingClientRect();
        if (!whole(box.left - frame.left) || !whole(box.width)) say(`"${name}" is ${box.width}px wide at ${box.left - frame.left}px`);
        if (lead) {
          if (style.position !== 'sticky') say(`"${name}" is not pinned`);
          if (Math.abs(box.left - port.left) > 0.01) say(`"${name}" sits ${box.left - port.left}px from the left edge`);
          if (!/^rgb\(/.test(style.backgroundColor)) say(`"${name}" is see-through: ${style.backgroundColor}`);
        }
        // The frame's right side is the right line of the last column.
        if (!lead && Math.abs(box.right - frame.right) < 0.01 && widths[1] !== 1) say(`"${name}" leaves the frame open`);
      }

      // The pinned column's edge: over its last pixel, down the whole table,
      // and only while the block is scrolled sideways.
      const edge = block.querySelector('.chart-edge');
      const at = edge.getBoundingClientRect();
      const column = parseFloat(table.querySelector('col').style.width);
      const shown = getComputedStyle(edge).opacity === '1';
      const hidden = getComputedStyle(edge).opacity === '0';
      if (scroller.scrollLeft > 0 ? !shown : !hidden) say(`the pinned edge is ${getComputedStyle(edge).opacity} at ${scroller.scrollLeft}px`);
      if (Math.abs(at.left - (port.left + column - 1)) > 0.01) say(`the pinned edge is at ${at.left - port.left}px beside a ${column}px column`);
      if (Math.abs(at.top - frame.top) > 0.01 || Math.abs(at.bottom - frame.bottom) > 0.01) {
        say(`the pinned edge runs ${at.top - frame.top}px to ${at.bottom - frame.bottom}px past the table`);
      }
      if (getComputedStyle(edge).pointerEvents !== 'none') say('the pinned edge takes taps');

      // Scrolled to the end, the frame's right line is on screen.
      const end = scroller.scrollWidth - scroller.clientWidth;
      if (end > 1 && scroller.scrollLeft >= end - 1 && frame.right > port.right - 1) say('the right line is cut off at the end');
    });
    return problems;
  });

async function checkLines(page, where) {
  const blocks = page.locator('[data-testid=chart] .chart-block');
  for (let index = 0; index < (await blocks.count()); index += 1) {
    const block = blocks.nth(index);
    for (const [label, left] of positions(await maxScroll(block))) {
      await scrollBlock(block, left);
      await expect
        .poll(() => lineProblems(page), { message: `${where}, block ${index + 1} scrolled to the ${label}` })
        .toEqual([]);
    }
    await scrollBlock(block, 0);
  }
}

test('every chart line is drawn once, and the pinned column keeps its edge while scrolling', async ({ page }) => {
  await page.goto('/');
  let scrolling = 0;
  let split = 0;
  // Every scroll position of every block takes a while: of a large library,
  // the recipes with the most steps (the widest charts) stand for the rest.
  const steps = (recipe) => recipe.doc.components.reduce((sum, component) => sum + component.steps.length, 0);
  const recipes = (await activeRecipes(page)).sort((a, b) => steps(b) - steps(a)).slice(0, 30);
  test.setTimeout(60_000 + recipes.length * 20_000);
  for (const recipe of recipes) {
    await openChart(page, recipe);
    await checkLines(page, recipe.doc.title);
    const chart = page.locator('[data-testid=chart]');
    scrolling += await chart
      .locator('.chart-scroll')
      .evaluateAll((all) => all.filter((scroller) => scroller.scrollWidth > scroller.clientWidth + 1).length);
    split += await chart.locator('td.fill').count();
    if (!(await openCookChart(page))) continue;
    // Marks restyle cells; they must not move or drop a line.
    await page.locator('[data-testid=chart] td.lead.leaf.tap').first().click();
    await page.locator('[data-testid=chart] td.step').first().click();
    await page.locator('.sheet.open [data-testid=sheet-done]').click();
    await checkLines(page, `${recipe.doc.title} in cook mode`);
    await discardCook(page);
  }
  // The library has what the rules are about: charts that scroll and inputs
  // that stretch to a later step.
  expect(scrolling).toBeGreaterThan(0);
  expect(split).toBeGreaterThan(0);
});

test('a ready step that stretches right has one ring, not one around each half', async ({ page }) => {
  await page.goto('/');
  let seen = 0;
  const recipes = await activeRecipes(page);
  test.setTimeout(60_000 + recipes.length * 10_000);
  for (const recipe of recipes) {
    await openChart(page, recipe);
    if (!(await openCookChart(page))) continue;
    const halves = page.locator('[data-testid=chart] td.lead.ready + td.fill.ready');
    for (let index = 0; index < (await halves.count()); index += 1) {
      const sides = (cell) => getComputedStyle(cell).boxShadow.split('inset').length - 1;
      const fill = halves.nth(index);
      expect(await fill.evaluate(sides)).toBe(3);
      expect(await fill.evaluate((cell) => getComputedStyle(cell.previousElementSibling).boxShadow)).not.toBe(
        await fill.evaluate((cell) => getComputedStyle(cell).boxShadow),
      );
      expect(await fill.evaluate((cell) => getComputedStyle(cell.previousElementSibling).boxShadow.split('inset').length - 1)).toBe(3);
      seen += 1;
    }
    await discardCook(page);
  }
  expect(seen).toBeGreaterThan(0);
});

// The phone's own frame first (what fits on the screen, with the bars), then
// every block whole on a tall page with the Cook button out of the way.
const PHONE = { width: 390, height: 664 };
const TALL = { width: 390, height: 2400 };

async function capture(page, name) {
  await page.setViewportSize(PHONE);
  const blocks = page.locator('[data-testid=chart] .chart-block');
  const last = blocks.last();
  await last.scrollIntoViewIfNeeded();
  await page.screenshot({ path: path.join(SHOTS, `${name}-phone-start.png`) });
  await scrollBlock(last, Math.round((await maxScroll(last)) / 2));
  await page.screenshot({ path: path.join(SHOTS, `${name}-phone-middle.png`) });
  await scrollBlock(last, 0);
  await page.setViewportSize(TALL);
  await captureBlocks(page, name);
  await page.setViewportSize(PHONE);
}

test('screenshots of every chart at each sideways scroll position', async ({ page }) => {
  test.skip(!SHOTS, 'set FOND_CHART_SHOTS to a directory');
  test.setTimeout(300_000);
  fs.mkdirSync(SHOTS, { recursive: true });
  await page.goto('/');
  for (const recipe of await activeRecipes(page)) {
    const name = slug(recipe.doc.title);
    await openChart(page, recipe);
    await page.addStyleTag({ content: '.cta { visibility: hidden; }' });
    await capture(page, `recipe-${name}`);
    await page.addStyleTag({ content: '.cta { visibility: visible; }' });
    if (!(await openCookChart(page))) continue;
    // One ingredient crossed off and one step done, so the marked styles are
    // in the picture too.
    await page.locator('[data-testid=chart] td.lead.leaf.tap').first().click();
    await page.locator('[data-testid=chart] td.step').first().click();
    await page.locator('.sheet.open [data-testid=sheet-done]').click();
    await capture(page, `cook-${name}`);
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.setViewportSize(TALL);
    const block = page.locator('[data-testid=chart] .chart-block').last();
    await scrollBlock(block, Math.round((await maxScroll(block)) / 2));
    await block.screenshot({ path: path.join(SHOTS, `cook-${name}-dark-middle.png`) });
    await page.setViewportSize(PHONE);
    await page.emulateMedia({ colorScheme: 'light' });
    await discardCook(page);
  }
});
