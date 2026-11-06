import { expect, test } from '@playwright/test';

test('preview file selection, removal and all navigation paths', async ({ page }, testInfo) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  await page.goto('/');
  await expect(page.getByText('浏览器预览 · 无原生连接')).toBeVisible();
  await expect(page.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
  await expect(page.getByRole('button', { name: '请求配对' })).toBeDisabled();
  await page.getByLabel('浏览器预览选择文件').setInputFiles([
    { name: '验证文件.txt', mimeType: 'text/plain', buffer: Buffer.from('local preview') },
    { name: '空文件.txt', mimeType: 'text/plain', buffer: Buffer.alloc(0) },
  ]);
  await expect(page.getByText('验证文件.txt', { exact: true })).toBeVisible();
  await expect(page.getByText('空文件.txt', { exact: true })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('send-page.png'), fullPage: true });
  await page.getByRole('button', { name: '移除 验证文件.txt' }).click();
  await expect(page.getByText('验证文件.txt', { exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: '传输记录', exact: true }).click();
  await expect(page.getByText('还没有传输任务')).toBeVisible();
  await page.getByRole('button', { name: '设备与安全', exact: true }).click();
  await expect(page.getByRole('button', { name: '运行检查' })).toBeDisabled();
  await page.getByRole('button', { name: '偏好设置', exact: true }).click();
  await expect(page.getByLabel('设备名称')).toBeDisabled();
  await expect(page.getByRole('button', { name: '保存设置' })).toBeDisabled();
  await page.getByRole('button', { name: '发送文件', exact: true }).click();
  await expect(page.getByText('空文件.txt', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '清空', exact: true }).click();
  await expect(page.getByText('空文件.txt', { exact: true })).toHaveCount(0);
  expect(errors).toEqual([]);
});

test('oversized selections show an actionable error without losing files', async ({ page }) => {
  await page.goto('/');
  const input = page.getByLabel('浏览器预览选择文件');
  await input.setInputFiles({
    name: 'keep.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('keep'),
  });
  await input.setInputFiles(
    Array.from({ length: 128 }, (_, index) => ({
      name: `${index}.txt`,
      mimeType: 'text/plain',
      buffer: Buffer.from('a'),
    })),
  );
  await expect(page.getByRole('alert')).toContainText('最多可选择 128');
  await expect(page.getByText('keep.txt', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '关闭错误提示' }).click();
  await expect(page.getByRole('alert')).toHaveCount(0);
});

test('narrow layout preserves navigation labels without horizontal overflow', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  await expect(page.getByRole('button', { name: '发送文件', exact: true })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.getByRole('button', { name: '偏好设置', exact: true }).click();
  await expect(page.getByLabel('设备名称')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath('narrow-settings.png'), fullPage: true });
});

test('preview never issues requests to other devices or cloud services', async ({ page }) => {
  const external: string[] = [];
  page.on('request', (request) => {
    const address = new URL(request.url());
    if (address.protocol.startsWith('http') && address.hostname !== '127.0.0.1')
      external.push(address.origin);
  });
  await page.goto('/');
  await page.getByRole('button', { name: '设备与安全', exact: true }).click();
  await expect(page.getByText('诊断需要在桌面应用中运行。')).toBeVisible();
  expect(external).toEqual([]);
});
