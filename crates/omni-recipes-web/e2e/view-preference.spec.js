// The recipe view (Steps or Chart) chosen last is remembered on the device
// and shared by the recipe page and cook mode.
const { test, expect } = require('@playwright/test');

const activeRecipes = async (page) =>
  (await (await page.request.get('/api/sync')).json()).recipes.filter((recipe) => recipe.state === 'active');

async function openRecipe(page, recipe) {
  await page.goto(`/r/${recipe.id}`);
  await expect(page.locator('main.recipe h1')).toHaveText(recipe.doc.title);
}

// Through the library, as a person would get to another recipe.
async function openFromLibrary(page, recipe) {
  await page.goto('/');
  await page.getByPlaceholder('Search recipes').fill(recipe.doc.title);
  await page
    .locator('[data-testid=grid] a.card')
    .filter({ has: page.locator('h3', { hasText: recipe.doc.title }) })
    .first()
    .click();
  await expect(page.locator('main.recipe h1')).toHaveText(recipe.doc.title);
}

const tab = (page, name) => page.getByRole('tab', { name, exact: true });
const stored = (page) => page.evaluate(() => localStorage.getItem('fond.view'));

async function expectView(page, view) {
  const [on, off] = view === 'chart' ? ['Chart', 'Steps'] : ['Steps', 'Chart'];
  await expect(tab(page, on)).toHaveAttribute('aria-selected', 'true');
  await expect(tab(page, off)).toHaveAttribute('aria-selected', 'false');
  if (view === 'chart') {
    await expect(page.locator('[data-testid=chart] table.chart').first()).toBeVisible();
    await expect(page.locator('.list .ingredients')).toHaveCount(0);
  } else {
    await expect(page.locator('.list .ingredients').first()).toBeVisible();
    await expect(page.locator('[data-testid=chart]')).toHaveCount(0);
  }
}

test('the view chosen last opens every recipe, after a reload too, until it is switched back', async ({ page }) => {
  await page.goto('/');
  const [first, second] = await activeRecipes(page);

  // Nothing stored: a recipe opens as steps.
  await openRecipe(page, first);
  expect(await stored(page)).toBeNull();
  await expectView(page, 'steps');

  await tab(page, 'Chart').click();
  await expectView(page, 'chart');
  expect(await stored(page)).toBe('chart');
  await page.reload();
  await expect(page.locator('main.recipe h1')).toHaveText(first.doc.title);
  await expectView(page, 'chart');
  await openFromLibrary(page, second);
  await expectView(page, 'chart');

  await tab(page, 'Steps').click();
  await expectView(page, 'steps');
  expect(await stored(page)).toBe('steps');
  await page.reload();
  await expect(page.locator('main.recipe h1')).toHaveText(second.doc.title);
  await expectView(page, 'steps');
  await openFromLibrary(page, first);
  await expectView(page, 'steps');
});

test('cook mode opens in the remembered view and its Steps and Chart tabs change it; Gather does not', async ({ page }) => {
  await page.goto('/');
  const recipes = await activeRecipes(page);
  let recipe;
  for (const candidate of recipes) {
    await openRecipe(page, candidate);
    // A session someone left open is not ours to discard.
    if ((await page.locator('[data-testid=cook]').innerText()).trim() === 'Cook') {
      recipe = candidate;
      break;
    }
  }
  test.skip(!recipe, 'every recipe has an open cook session');

  await tab(page, 'Chart').click();
  await page.locator('[data-testid=cook]').click();
  await expect(page).toHaveURL(/\/cook\/cook_[a-z0-9]{8}$/);
  await expect(tab(page, 'Chart')).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('[data-testid=chart] table.chart').first()).toBeVisible();

  await tab(page, 'Gather').click();
  await expect(tab(page, 'Gather')).toHaveAttribute('aria-selected', 'true');
  expect(await stored(page)).toBe('chart');
  await page.reload();
  await expect(tab(page, 'Chart')).toHaveAttribute('aria-selected', 'true');

  await tab(page, 'Steps').click();
  expect(await stored(page)).toBe('steps');
  await page.reload();
  await expect(tab(page, 'Steps')).toHaveAttribute('aria-selected', 'true');

  await page.locator('[data-testid=leave]').click();
  await page.locator('[data-testid=leave-discard]').click();
  await page.locator('[data-testid=leave-discard-confirm]').click();
  await expect(page.locator('main.recipe h1')).toHaveText(recipe.doc.title);
  await expect(page.locator('[data-testid=connection]')).toHaveCount(0, { timeout: 20_000 });
  await expectView(page, 'steps');
});

for (const [name, block] of [
  [
    'refuses to be read',
    () => {
      Object.defineProperty(window, 'localStorage', {
        configurable: true,
        get() {
          throw new DOMException('The operation is insecure.', 'SecurityError');
        },
      });
    },
  ],
  [
    'throws on every call',
    () => {
      const refuse = () => {
        throw new DOMException('The quota has been exceeded.', 'QuotaExceededError');
      };
      Storage.prototype.getItem = refuse;
      Storage.prototype.setItem = refuse;
    },
  ],
]) {
  test(`storage that ${name} leaves the default view and a working toggle`, async ({ page }) => {
    const errors = [];
    // A reload can interrupt a fetch in flight (the service worker's own, or
    // the sync the app starts when its window gets the focus), which WebKit
    // reports as a failed load. That is not the app failing.
    const interrupted = /sw\.js|Fetch API cannot load .* due to access control checks/;
    page.on('pageerror', (error) => interrupted.test(String(error)) || errors.push(String(error)));
    await page.addInitScript(block);
    await page.goto('/');
    const [recipe] = await activeRecipes(page);
    await openRecipe(page, recipe);
    // The stand-in really is in the way of the app.
    expect(
      await page.evaluate(() => {
        try {
          localStorage.setItem('fond.probe', '1');
          return localStorage.getItem('fond.probe');
        } catch (error) {
          return 'refused';
        }
      }),
    ).toBe('refused');
    await expectView(page, 'steps');
    await tab(page, 'Chart').click();
    await expectView(page, 'chart');
    await page.reload();
    await expect(page.locator('main.recipe h1')).toHaveText(recipe.doc.title);
    await expectView(page, 'steps');
    expect(errors).toEqual([]);
  });
}
