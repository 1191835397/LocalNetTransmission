import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';
import type { DiscoveredDevice } from '../src/types';

type DiscoveryFixture = {
  devices: DiscoveredDevice[];
  scanningUntil: number;
  failRefresh: boolean;
  offline: boolean;
  calls: { command: string; args: Record<string, unknown> }[];
};
type FixtureWindow = typeof window & { discoveryFixture: DiscoveryFixture };

async function installDiscoveryFixture(page: Page) {
  await page.addInitScript(() => {
    const fixture: DiscoveryFixture = {
      devices: [
        {
          name: '发现测试电脑',
          fingerprint: Array(32).fill('BB').join(':'),
          platform: 'windows',
          addresses: ['192.168.1.3', '10.0.0.8'],
          port: 53318,
        },
      ],
      scanningUntil: 0,
      failRefresh: false,
      offline: false,
      calls: [],
    };
    Object.assign(window, {
      isTauri: true,
      discoveryFixture: fixture,
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener: () => {} },
      __TAURI_INTERNALS__: {
        metadata: {
          currentWindow: { label: 'main' },
          currentWebview: { label: 'main', windowLabel: 'main' },
        },
        transformCallback: () => 1,
        unregisterCallback: () => {},
        invoke: async (command: string, args: Record<string, unknown> = {}) => {
          fixture.calls.push({ command, args });
          if (command.startsWith('plugin:event|')) return 1;
          if (command === 'get_snapshot')
            return {
              version: '0.1.0',
              device: {
                id: 'fixture',
                name: '本机测试电脑',
                platform: 'windows',
                arch: 'x86_64',
                addresses: ['192.168.1.2'],
              },
              identity: {
                fingerprint: Array(32).fill('AA').join(':'),
                createdAt: '2026-11-15T00:00:00Z',
                storage: 'test fixture',
              },
              settings: { deviceName: '本机测试电脑', receiveDir: 'C:\\Downloads', port: 53318 },
              service: {
                state: 'running',
                address: '0.0.0.0',
                port: 53318,
                error: null,
                scope: 'lan',
              },
            };
          if (command === 'get_transfer_snapshot')
            return { peers: [], pairRequests: [], tasks: [] };
          if (command === 'discover_devices') {
            if (fixture.failRefresh) throw new Error('测试：mDNS 无法重新发现');
            fixture.scanningUntil = Date.now() + 1500;
          }
          if (command === 'get_discovery_snapshot' || command === 'discover_devices')
            return {
              state: fixture.offline ? 'error' : 'running',
              scanning: fixture.scanningUntil > Date.now(),
              devices: fixture.devices,
              error: fixture.offline ? '测试：mDNS 不可用' : null,
            };
          if (command === 'connect_peer') throw new Error('测试：对方拒绝配对');
          throw new Error(`未设置测试命令 ${command}`);
        },
      },
    });
  });
}

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
  await expect(page.getByRole('button', { name: '发现本地设备' })).toBeDisabled();
  await expect(page.getByText('浏览器无法发现局域网设备')).toBeVisible();
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

test('discovery UI fixture refreshes and requires explicit verified pairing', async ({
  page,
}, testInfo) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  await installDiscoveryFixture(page);
  await page.goto('/');
  await expect(page.getByText('自动发现已开启')).toBeVisible();
  await expect(page.getByText('发现测试电脑', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '发现本地设备' }).click();
  await expect(page.getByRole('button', { name: '正在发现设备…' })).toBeDisabled();
  await expect(page.getByRole('button', { name: '发现本地设备' })).toBeEnabled();
  await page.screenshot({ path: testInfo.outputPath('discovery-devices.png'), fullPage: true });
  await page.getByRole('button', { name: '连接 发现测试电脑 (10.0.0.8:53318)' }).click();
  await expect(page.getByLabel('对方 IPv4 地址')).toHaveValue('10.0.0.8');
  await expect(page.getByLabel('对方完整 SHA-256 指纹')).toHaveValue(
    Array(32).fill('BB').join(':'),
  );
  await expect(page.getByRole('button', { name: '请求配对' })).toBeDisabled();
  expect(
    await page.evaluate(() =>
      (window as FixtureWindow).discoveryFixture.calls.filter((c) => c.command === 'connect_peer'),
    ),
  ).toHaveLength(0);
  await page.getByLabel('已通过对方屏幕或可信渠道核对完整指纹').check();
  await page.getByRole('button', { name: '请求配对' }).click();
  await expect(page.getByRole('alert')).toContainText('对方拒绝配对');
  const requests = await page.evaluate(() =>
    (window as FixtureWindow).discoveryFixture.calls.filter((c) => c.command === 'connect_peer'),
  );
  expect(requests).toHaveLength(1);
  expect(requests[0].args.address).toBe('10.0.0.8');
  expect(errors).toEqual([]);
});

test('discovery UI fixture handles errors, expiry and narrow screens', async ({
  page,
}, testInfo) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await installDiscoveryFixture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  await expect(page.getByText('发现测试电脑', { exact: true })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.evaluate(() => {
    (window as FixtureWindow).discoveryFixture.failRefresh = true;
  });
  await page.getByRole('button', { name: '发现本地设备' }).click();
  await expect(page.getByRole('alert')).toContainText('无法重新发现');
  await expect(page.getByRole('button', { name: '发现本地设备' })).toBeEnabled();
  await page.evaluate(() => {
    const fixture = (window as FixtureWindow).discoveryFixture;
    fixture.failRefresh = false;
    fixture.devices = [];
  });
  await page.getByRole('button', { name: '发现本地设备' }).click();
  await expect(page.getByText('暂未发现附近设备')).toBeVisible();
  await expect(page.getByRole('alert')).toHaveCount(0);
  await expect(page.getByText('发现测试电脑', { exact: true })).toHaveCount(0);
  await expect(page.getByLabel('对方 IPv4 地址')).toBeEnabled();
  await page.screenshot({
    path: testInfo.outputPath('discovery-empty-narrow.png'),
    fullPage: true,
  });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(errors).toEqual([]);
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
