import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { chromium } from '@playwright/test';
import { nativeEnv, root } from './native-env.mjs';

if (process.platform !== 'win32') throw new Error('此冒烟脚本仅适用于 Windows WebView2。');
const profile = process.argv.includes('--release') ? 'release' : 'debug';
const executable = path.join(root, 'src-tauri', 'target', profile, 'lanlink.exe');
const output = path.join(
  root,
  '.tools',
  profile === 'release' ? 'native-smoke-release' : 'native-smoke',
);
await mkdir(output, { recursive: true });
const sample = path.join(output, 'native-sample.txt');
await writeFile(sample, 'LanLink native smoke test\n');

async function freePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function startApp() {
  const debugPort = await freePort();
  const child = spawn(executable, [], {
    cwd: root,
    env: {
      ...nativeEnv(),
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${debugPort} --remote-debugging-address=127.0.0.1`,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let launchError;
  child.on('error', (error) => {
    launchError = error;
  });
  child.stdout.on('data', (data) => process.stdout.write(data));
  child.stderr.on('data', (data) => process.stderr.write(data));
  const deadline = Date.now() + 40_000;
  let browser;
  try {
    while (Date.now() < deadline) {
      if (launchError) throw launchError;
      if (child.exitCode !== null) throw new Error(`桌面进程提前退出：${child.exitCode}`);
      try {
        browser = await chromium.connectOverCDP(`http://127.0.0.1:${debugPort}`, { timeout: 1000 });
        break;
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
    }
    if (!browser) throw new Error('无法连接本次启动的 WebView2 调试端口。');
    let page;
    while (Date.now() < deadline) {
      page = browser
        .contexts()
        .flatMap((context) => context.pages())
        .find((candidate) => candidate.url().includes('tauri.localhost'));
      if (page) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (!page) throw new Error('未找到已打包的 Tauri 应用页面。');
    await page.waitForSelector('h1');
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    return { child, browser, page, errors };
  } catch (error) {
    if (browser) await browser.close();
    child.kill();
    throw error;
  }
}

async function stopApp(app) {
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-Command',
      `$process = Get-Process -Id ${app.child.pid}; if (-not $process.CloseMainWindow()) { throw "无法关闭本次启动的应用窗口" }`,
    ],
    { stdio: 'inherit' },
  );
  await Promise.race([
    new Promise((resolve) =>
      app.child.exitCode !== null ? resolve() : app.child.once('exit', resolve),
    ),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error('关闭主窗口后进程未退出')), 6000).unref(),
    ),
  ]);
  await app.browser.close();
}

const invoke = (page, command, args = {}) =>
  page.evaluate(({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), {
    command,
    args,
  });
const report = [];
let app;
let original;
let restored = true;
try {
  app = await startApp();
  const first = await invoke(app.page, 'get_snapshot');
  original = first.settings;
  assert.match(first.identity.fingerprint, /^(?:[A-F0-9]{2}:){31}[A-F0-9]{2}$/);
  assert.equal(first.service.state, 'running', first.service.error ?? 'TLS 服务未运行');
  assert.equal(first.service.scope, 'lan');
  assert.equal(first.service.address, '0.0.0.0');
  report.push('本机快照、系统身份、P2 TLS 监听服务启动通过');
  const diagnostics = await invoke(app.page, 'run_diagnostics');
  assert.equal(
    diagnostics.some((item) => item.status === 'fail'),
    false,
    JSON.stringify(diagnostics),
  );
  report.push('真实 TLS 请求、证书验证及本机诊断通过');

  const inspected = await invoke(app.page, 'inspect_files', { paths: [sample, sample] });
  assert.equal(inspected.length, 1);
  assert.equal(inspected[0].name, 'native-sample.txt');
  assert.equal(inspected[0].size, Buffer.byteLength('LanLink native smoke test\n'));
  await assert.rejects(invoke(app.page, 'inspect_files', { paths: [output] }));
  await assert.rejects(invoke(app.page, 'inspect_files', { paths: ['relative.txt'] }));
  report.push('真实文件元数据、去重及非法选择拒绝通过');
  await app.page.getByRole('button', { name: '选择文件', exact: true }).click();
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-File',
      path.join(root, 'scripts', 'native-file-dialog.ps1'),
      '-ApplicationPid',
      String(app.child.pid),
      '-FilePath',
      sample,
    ],
    { stdio: 'inherit', timeout: 20_000 },
  );
  await app.page.getByRole('button', { name: '移除 native-sample.txt', exact: true }).waitFor();
  await app.page.screenshot({ path: path.join(output, 'native-send.png') });
  report.push('Windows 原生文件选择对话框与界面文件列表通过');

  await app.page.getByRole('button', { name: '偏好设置', exact: true }).click();
  await app.page.getByLabel('设备名称').fill('LanLink 原生验证');
  const updatedPort = first.service.port === 53319 ? 53320 : 53319;
  await app.page.getByLabel('本机服务端口').fill(String(updatedPort));
  restored = false;
  await app.page.getByRole('button', { name: '保存设置', exact: true }).click();
  await app.page.getByRole('status').waitFor();
  const changed = await invoke(app.page, 'get_snapshot');
  assert.equal(changed.settings.deviceName, 'LanLink 原生验证');
  assert.equal(changed.settings.port, updatedPort);
  assert.equal(changed.service.port, first.service.port);
  await assert.rejects(invoke(app.page, 'save_settings', { settings: { ...original, port: 80 } }));
  assert.equal((await invoke(app.page, 'get_snapshot')).settings.port, updatedPort);
  report.push('原生界面保存、参数校验及端口延迟生效通过');
  await app.page.screenshot({ path: path.join(output, 'native-settings.png') });
  await app.page.getByRole('button', { name: '设备与安全', exact: true }).click();
  await app.page.getByRole('button', { name: '运行检查', exact: true }).click();
  await app.page.getByText('本机 TLS 健康检查', { exact: true }).waitFor();
  await app.page.screenshot({ path: path.join(output, 'native-security.png') });
  assert.deepEqual(app.errors, []);
  await stopApp(app);
  app = undefined;

  app = await startApp();
  const restarted = await invoke(app.page, 'get_snapshot');
  assert.equal(restarted.identity.fingerprint, first.identity.fingerprint);
  assert.equal(restarted.settings.deviceName, 'LanLink 原生验证');
  assert.equal(restarted.service.port, updatedPort);
  report.push('重启后身份和设置持久化、新端口生效通过');
  await invoke(app.page, 'save_settings', { settings: original });
  restored = true;
  await stopApp(app);
  app = undefined;
  report.push('测试修改已恢复，主窗口关闭和服务退出通过');
  const occupied = createServer((socket) => socket.destroy());
  await new Promise((resolve, reject) => {
    occupied.once('error', reject);
    occupied.listen(original.port, '127.0.0.1', resolve);
  });
  try {
    app = await startApp();
    const conflict = await invoke(app.page, 'get_snapshot');
    assert.equal(conflict.service.state, 'error');
    assert.match(conflict.service.error, /端口可能被占用/);
    await app.page.getByText('本机服务待检查', { exact: true }).waitFor();
    await app.page.getByRole('button', { name: '设备与安全', exact: true }).click();
    const failedChecks = await invoke(app.page, 'run_diagnostics');
    assert.equal(
      failedChecks.some((item) => item.name === '本机 TLS 健康检查' && item.status === 'fail'),
      true,
    );
    await stopApp(app);
    app = undefined;
    report.push('真实端口占用时窗口仍可用、服务状态和诊断正确报错');
  } finally {
    await new Promise((resolve) => occupied.close(resolve));
  }
  console.log(report.join('\n'));
  await writeFile(
    path.join(output, 'result.json'),
    JSON.stringify({ profile, passed: report, checkedAt: new Date().toISOString() }, null, 2),
  );
} finally {
  if (app) {
    if (!restored && original) {
      await invoke(app.page, 'save_settings', { settings: original }).catch((error) =>
        console.error('恢复设置失败：', error),
      );
    }
    await app.browser.close().catch(() => {});
    if (app.child.exitCode === null) app.child.kill();
  }
}
