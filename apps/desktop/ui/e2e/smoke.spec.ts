import { expect, test } from '@playwright/test';

test('the shell shows the app name, mock version and mock label', async ({ page }) => {
  await page.goto('/');

  await expect(page.getByRole('heading', { name: 'CK Dev' })).toBeVisible();
  await expect(page.getByText('0.0.0-mock')).toBeVisible();
  await expect(page.getByText('Mock data')).toBeVisible();
});
