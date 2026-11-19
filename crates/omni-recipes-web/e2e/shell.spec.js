// The page shell: the document never scrolls, so a screen's header and footer
// cannot be pulled with the content. Each screen scrolls in one area between
// them, and that area, the chart and the sheets keep their own scrolling.
//
// What these tests cannot see is the bounce itself: Playwright's WebKit has
// no rubber band, no touch drag, no wheel at phone size and no on-screen
// keyboard. They check the structure that decides the behaviour (what can
// scroll, what is fixed, what follows the keyboard): a pull is played by
// scrolling whatever lies under the finger as far as it will go, and the
// keyboard by shrinking the visual viewport.
const { test, expect } = require('@playwright/test');

const RECIPES = {
  wide: process.env.FOND_RECIPE_CHART || 'Lasagne alla Bolognese',
  cook: process.env.FOND_RECIPE_COOK || 'Chicken Piccata',
  amount: process.env.FOND_RECIPE_AMOUNT || 'Mustard Vinaigrette',
  ingredient: process.env.FOND_AMOUNT_INGREDIENT || 'salt',
};
const KEYBOARD = 300;

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
  await expect(page.locator('main.recipe h1')).toContainText(title);
}

const box = (page, selector) =>
  page.evaluate((sel) => {
    const rect = document.querySelector(sel).getBoundingClientRect();
    return { top: Math.round(rect.top), bottom: Math.round(rect.bottom), height: Math.round(rect.height) };
  }, selector);

const area = (page) =>
  page.evaluate(() => {
    const el = document.querySelector('.scroller');
    return { top: el.scrollTop, max: el.scrollHeight - el.clientHeight, doc: window.scrollY };
  });

// Scrolls what lies under the point (x, y) by `dy`, the way a drag there
// would: the nearest scrolling ancestor takes it.
const drag = (page, x, y, dy) =>
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

// Pulls the content further than it can go, both ways, and asks the document
// itself to scroll. `fixed` must not move at any point.
async function pullPastBothEnds(page, fixed) {
  const before = await Promise.all(fixed.map((selector) => box(page, selector)));
  const still = async () => {
    expect(await Promise.all(fixed.map((selector) => box(page, selector)))).toEqual(before);
    expect((await area(page)).doc).toBe(0);
  };
  const size = page.viewportSize();
  const pull = (dy) => drag(page, size.width / 2, size.height / 2, dy);
  await pull(-900);
  await still();
  expect((await area(page)).top).toBe(0);
  await pull(100000);
  await expect.poll(async () => (await area(page)).top).toBeGreaterThan(0);
  await still();
  const { top, max } = await area(page);
  expect(Math.abs(top - max)).toBeLessThanOrEqual(1);
  await pull(900);
  await still();
  await page.evaluate(() => {
    window.scrollTo(0, 400);
    document.documentElement.scrollTop = 400;
    document.body.scrollTop = 400;
  });
  await still();
  await page.evaluate(() => document.querySelector('.scroller').scrollTo(0, 0));
}

// The on-screen keyboard: the visual viewport loses `height` at the bottom.
async function keyboard(page, height) {
  await page.evaluate((covered) => {
    const viewport = window.visualViewport;
    Object.defineProperty(viewport, 'height', { configurable: true, get: () => window.innerHeight - covered });
    viewport.dispatchEvent(new Event('resize'));
  }, height);
}

// Whether the focused field is wholly on the part of the screen the keyboard leaves.
const fieldAboveKeyboard = (page) =>
  page.evaluate((covered) => {
    const rect = document.activeElement.getBoundingClientRect();
    return rect.top >= 0 && rect.bottom <= window.innerHeight - covered + 1;
  }, KEYBOARD);

test('the document cannot scroll and every screen has one area that always can', async ({ page }) => {
  await openLibrary(page);
  const lock = await page.evaluate(() => {
    const [html, body] = [getComputedStyle(document.documentElement), getComputedStyle(document.body)];
    return {
      html: [html.overflowY, html.overscrollBehaviorY],
      body: [body.overflowY, body.overscrollBehaviorY, body.position],
      fits: document.documentElement.scrollHeight <= window.innerHeight,
    };
  });
  expect(lock).toEqual({ html: ['hidden', 'none'], body: ['hidden', 'none', 'fixed'], fits: true });
  const scroller = await page.evaluate(() => {
    const style = getComputedStyle(document.querySelector('.scroller'));
    return [style.overflowY, style.overflowX, style.overscrollBehaviorY];
  });
  expect(scroller).toEqual(['auto', 'hidden', 'contain']);
  expect(await page.locator('.scroller').count()).toBe(1);
  // Even a screen with little on it scrolls, so iOS bounces it and not the shell.
  await page.getByPlaceholder('Search recipes').fill('zzzz no recipe has this name');
  await expect(page.locator('[data-testid=grid] a.card')).toHaveCount(0);
  expect((await area(page)).max).toBeGreaterThanOrEqual(1);
});

test('library: the header stays put when the content is pulled past its top and bottom', async ({ page }) => {
  await openLibrary(page);
  await pullPastBothEnds(page, ['main.library .bar']);
  expect((await box(page, 'main.library .bar')).top).toBe(0);
});

test('recipe: the header and the Cook button stay put, and the chart still scrolls sideways', async ({ page }) => {
  await openRecipe(page, RECIPES.wide);
  await pullPastBothEnds(page, ['main.recipe .bar', 'main.recipe .cta']);
  const size = page.viewportSize();
  expect((await box(page, 'main.recipe .cta')).bottom).toBe(size.height);

  await page.getByRole('tab', { name: 'Chart', exact: true }).click();
  const wide = await page.evaluate(() => {
    const scrollers = [...document.querySelectorAll('.chart-scroll')];
    const index = scrollers.findIndex((el) => el.scrollWidth > el.clientWidth + 1);
    if (index < 0) return null;
    scrollers[index].scrollIntoView({ block: 'center' });
    scrollers[index].scrollLeft = 120;
    const style = getComputedStyle(scrollers[index]);
    return { left: scrollers[index].scrollLeft, bounce: style.overscrollBehaviorX, overflow: style.overflowX };
  });
  test.skip(!wide, 'no chart of this recipe is wider than the screen');
  // Its own sideways scrolling and bounce are left alone.
  expect(wide.left).toBeGreaterThan(0);
  expect([wide.bounce, wide.overflow]).toEqual(['auto', 'auto']);
  // The pinned ingredient column still gets its edge while scrolled.
  await expect(page.locator('.chart-block.scrolled').first()).toBeVisible();
  expect((await area(page)).doc).toBe(0);
});

test('tapping the header scrolls the screen to its top, and its buttons do not', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 420 });
  await openRecipe(page, RECIPES.wide);
  const down = Math.min(300, (await area(page)).max);
  expect(down).toBeGreaterThan(50);
  await page.evaluate((top) => document.querySelector('.scroller').scrollTo(0, top), down);
  // The middle of the bar is bare; Back is on its left and More on its right.
  const bar = await page.locator('main.recipe .bar').boundingBox();
  await page.mouse.click(bar.x + bar.width / 2, bar.y + bar.height - 20);
  await expect.poll(async () => (await area(page)).top).toBe(0);

  await page.evaluate((top) => document.querySelector('.scroller').scrollTo(0, top), down);
  await page.locator('main.recipe .bar').getByRole('button', { name: 'More' }).click();
  await expect(page.locator('.sheet.open')).toBeVisible();
  expect((await area(page)).top).toBe(down);
});

test('a sheet scrolls by itself and takes nothing behind it along', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 420 });
  await openLibrary(page);
  const before = await area(page);
  await page.getByRole('button', { name: 'Filter' }).click();
  const sheet = page.locator('.sheet.open');
  await expect(sheet).toBeVisible();
  expect(await sheet.evaluate((el) => [getComputedStyle(el).overflowY, getComputedStyle(el).overscrollBehaviorY])).toEqual([
    'auto',
    'contain',
  ]);
  const rect = await sheet.boundingBox();
  // A pull on the sheet, and on the scrim above it, reaches nothing behind.
  await drag(page, rect.x + rect.width / 2, rect.y + rect.height / 2, 100000);
  await drag(page, rect.x + rect.width / 2, rect.y + rect.height / 2, -100000);
  await drag(page, rect.x + rect.width / 2, Math.max(2, rect.y / 2), 100000);
  expect(await area(page)).toEqual(before);
});

test.describe('with the keyboard up', () => {
  test('search: the screen ends at the keyboard and the field stays in view', async ({ page }) => {
    await openLibrary(page);
    await page.getByPlaceholder('Search recipes').focus();
    await keyboard(page, KEYBOARD);
    const size = page.viewportSize();
    await expect.poll(async () => (await box(page, 'main.library')).bottom).toBe(size.height - KEYBOARD);
    expect(await fieldAboveKeyboard(page)).toBe(true);
    expect((await box(page, 'main.library .bar')).top).toBe(0);
    expect((await area(page)).doc).toBe(0);
    // Results under the field still scroll in what is left of the screen.
    expect((await area(page)).max).toBeGreaterThanOrEqual(1);
    await keyboard(page, 0);
    await expect.poll(async () => (await box(page, 'main.library')).bottom).toBe(size.height);
  });

  test('recipe notes: the field is brought above the keyboard and the Cook button gets out of the way', async ({ page }) => {
    await openRecipe(page, RECIPES.amount);
    const notes = page.locator('main.recipe').getByLabel('Notes');
    await notes.scrollIntoViewIfNeeded();
    await notes.focus();
    await keyboard(page, KEYBOARD);
    await expect(page.locator('[data-testid=cook]')).toBeHidden();
    await expect.poll(() => fieldAboveKeyboard(page)).toBe(true);
    expect((await area(page)).doc).toBe(0);
    await keyboard(page, 0);
    await expect(page.locator('[data-testid=cook]')).toBeVisible();
  });

  test('the amount sheet in the recipe sits on the keyboard with its field in view', async ({ page }) => {
    await openRecipe(page, RECIPES.amount);
    await page.locator(`li[data-ingredient="${RECIPES.ingredient}"] [data-testid=edit-amount]`).click();
    const sheet = page.locator('.sheet.open');
    await sheet.locator('[data-testid=amount-qty]').focus();
    await keyboard(page, KEYBOARD);
    const size = page.viewportSize();
    await expect.poll(async () => Math.round((await sheet.boundingBox()).y + (await sheet.boundingBox()).height)).toBe(
      size.height - KEYBOARD,
    );
    expect((await sheet.boundingBox()).y).toBeGreaterThanOrEqual(0);
    expect(await fieldAboveKeyboard(page)).toBe(true);
    await expect(sheet.locator('[data-testid=amount-save]')).toBeVisible();
    await keyboard(page, 0);
    await sheet.getByRole('button', { name: 'Cancel' }).click();
  });

  test('the amount sheet and the recipe notes in cook mode do the same', async ({ page }) => {
    await openRecipe(page, RECIPES.amount);
    test.skip((await page.locator('[data-testid=cook]').innerText()).trim() !== 'Cook', 'a cook of this recipe is open');
    await page.locator('[data-testid=cook]').click();
    await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
    try {
      // Cook mode's header and tabs are outside the scrolling area.
      await pullPastBothEnds(page, ['main.cook .bar', 'main.cook .tabs']);
      const size = page.viewportSize();
      await page.getByRole('tab', { name: 'Gather' }).click();
      const row = page.locator('.gather li').filter({ has: page.locator(`.check[data-target="${RECIPES.ingredient}"]`) });
      await row.locator('[data-testid=edit-amount]').click();
      const sheet = page.locator('.sheet.open');
      await sheet.locator('[data-testid=amount-qty]').focus();
      await keyboard(page, KEYBOARD);
      await expect.poll(async () => Math.round((await sheet.boundingBox()).y + (await sheet.boundingBox()).height)).toBe(
        size.height - KEYBOARD,
      );
      expect(await fieldAboveKeyboard(page)).toBe(true);
      await keyboard(page, 0);
      await sheet.getByRole('button', { name: 'Cancel' }).click();

      await page.getByRole('tab', { name: 'Steps' }).click();
      await page.locator('[data-testid=recipe-notes]').click();
      const notes = page.locator('.sheet.open').getByLabel('Notes');
      await notes.focus();
      await keyboard(page, KEYBOARD);
      await expect.poll(() => fieldAboveKeyboard(page)).toBe(true);
      await keyboard(page, 0);
    } finally {
      // Discard the cook: nothing is logged.
      await page.keyboard.press('Escape');
      const close = page.locator('.sheet.open').getByRole('button', { name: 'Close' });
      if (await close.count()) await close.first().click();
      await page.locator('[data-testid=leave]').click();
      await page.locator('[data-testid=leave-discard]').click();
      await page.locator('[data-testid=leave-discard-confirm]').click();
      await expect(page.locator('main.recipe h1')).toBeVisible();
    }
  });
});
