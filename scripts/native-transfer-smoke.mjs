import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtemp, readFile, writeFile, rename, mkdir } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createServer } from 'node:net';
import path from 'node:path';
import { chromium, expect } from '@playwright/test';
import { nativeEnv, root } from './native-env.mjs';

if (process.platform !== 'win32') throw new Error('此脚本仅验证 Windows 原生应用。');
const profile = process.argv.includes('--release') ? 'release' : 'debug';
const output = await mkdtemp(path.join(root, '.tools', `native-p2-${profile}-`));
const incoming = path.join(output, 'received');
await mkdir(incoming);
const source = path.join(output, '原生 sample.bin');
const content = Buffer.alloc(1024 * 1024);
for (let index = 0; index < content.length; index++) content[index] = index % 251;
await writeFile(source, content);
const digest = (buffer) => createHash('sha256').update(buffer).digest('hex');

async function until(check, description, milliseconds = 60_000) {
  const deadline = Date.now() + milliseconds;
  while (Date.now() < deadline) {
    const value = await check();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`等待超时：${description}`);
}

async function json(file) {
  try {
    return JSON.parse(await readFile(file, 'utf8'));
  } catch (error) {
    if (error.code === 'ENOENT') return null;
    throw error;
  }
}

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

let sequence = 0;
async function peerCommand(action, args = {}) {
  const current = ++sequence;
  await writeFile(
    path.join(output, 'command.tmp'),
    JSON.stringify({ sequence: current, action, ...args }),
  );
  await rename(path.join(output, 'command.tmp'), path.join(output, 'command.json'));
  if (action === 'stop') return;
  const reply = await until(async () => {
    const value = await json(path.join(output, 'reply.json'));
    return value?.sequence === current ? value : null;
  }, `对端命令 ${action}`);
  return reply.result;
}

const fixture = spawn(
  process.execPath,
  [
    'scripts/cargo.mjs',
    'test',
    '--manifest-path',
    'src-tauri/Cargo.toml',
    'p2_tests::native_peer_fixture',
    '--',
    '--ignored',
    '--exact',
    '--nocapture',
  ],
  {
    cwd: root,
    env: { ...nativeEnv(), LANLINK_FIXTURE_DIR: output },
    stdio: 'inherit',
  },
);
let app;
let browser;
let page;
let peer;
const errors = [];
const report = [];
const invoke = (command, args = {}) =>
  page.evaluate(({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), {
    command,
    args,
  });
const terminal = (task) =>
  ['completed', 'failed', 'cancelled', 'rejected', 'unknown'].includes(task.status);
function closeWindow() {
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-Command',
      `$p=Get-Process -Id ${app.pid}; if (-not $p.CloseMainWindow()) { throw 'Unable to close test app' }`,
    ],
    { stdio: 'inherit' },
  );
}

function confirmExit(buttonText) {
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-File',
      path.join(root, 'scripts', 'native-confirm-dialog.ps1'),
      '-ApplicationPid',
      String(app.pid),
      '-ButtonText',
      buttonText,
    ],
    { stdio: 'inherit', timeout: 20_000 },
  );
}

async function launchApp() {
  const debugPort = await freePort();
  app = spawn(path.join(root, 'src-tauri', 'target', profile, 'lanlink.exe'), [], {
    cwd: root,
    env: {
      ...nativeEnv(),
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${debugPort} --remote-debugging-address=127.0.0.1`,
    },
    stdio: 'inherit',
  });
  browser = await until(async () => {
    if (app.exitCode !== null) throw new Error(`原生应用提前退出：${app.exitCode}`);
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${debugPort}`, { timeout: 1000 });
    } catch {
      return null;
    }
  }, '本次应用 WebView2');
  page = await until(
    () =>
      browser
        .contexts()
        .flatMap((context) => context.pages())
        .find((candidate) => candidate.url().includes('tauri.localhost')),
    '应用页面',
  );
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  await page.waitForSelector('h1');
}

async function revokeTestPeer() {
  await page.getByRole('button', { name: '发送文件', exact: true }).click();
  await page.getByRole('button', { name: '撤销信任 P2 原生验证对端', exact: true }).click();
  await page.getByRole('button', { name: '确认撤销信任', exact: true }).click();
  await expect(page.getByRole('radio', { name: '选择接收设备 P2 原生验证对端' })).toHaveCount(0);
}

try {
  peer = await until(() => json(path.join(output, 'peer.json')), '独立 Rust 测试对端启动', 180_000);
  await launchApp();
  const local = await invoke('get_snapshot');
  assert.equal(local.service.state, 'running', local.service.error);
  assert.notEqual(peer.fingerprint, local.identity.fingerprint);
  const verificationLabel = '已通过对方屏幕或可信渠道核对完整指纹';
  await page.getByLabel('对方 IPv4 地址', { exact: true }).fill('127.0.0.1');
  await page.getByLabel('对方端口', { exact: true }).fill(String(peer.port));
  await page.getByLabel('对方完整 SHA-256 指纹', { exact: true }).fill(peer.fingerprint);
  await expect(page.getByRole('button', { name: '请求配对', exact: true })).toBeDisabled();
  await page.getByLabel(verificationLabel, { exact: true }).check();
  await page.getByRole('button', { name: '请求配对', exact: true }).click();
  await until(async () => (await peerCommand('snapshot')).pairRequests.length, '对端显示配对请求');
  await peerCommand('approvePair', { fingerprint: local.identity.fingerprint });
  await expect(page.getByRole('radio', { name: '选择接收设备 P2 原生验证对端' })).toBeVisible();
  assert.equal(
    (await invoke('get_transfer_snapshot')).peers.some(
      (item) => item.fingerprint === peer.fingerprint,
    ),
    true,
  );
  report.push('真实桌面身份与独立 Rust 身份双向 TLS 配对通过');

  await page.getByRole('button', { name: '发送文件', exact: true }).click();
  await page.getByRole('button', { name: '选择文件', exact: true }).click();
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-File',
      path.join(root, 'scripts', 'native-file-dialog.ps1'),
      '-ApplicationPid',
      String(app.pid),
      '-FilePath',
      source,
    ],
    { stdio: 'inherit', timeout: 20_000 },
  );
  await page.getByRole('button', { name: '移除 原生 sample.bin', exact: true }).waitFor();
  await page.getByRole('radio', { name: '选择接收设备 P2 原生验证对端' }).check();
  await page.getByRole('button', { name: '发送所选文件', exact: true }).click();
  const offered = await until(
    async () => (await peerCommand('snapshot')).tasks.find((task) => task.direction === 'receive'),
    '对端接收请求',
  );
  const id = offered.id;
  await peerCommand('accept', { id });
  const sent = await until(async () => {
    const task = (await invoke('get_transfer_snapshot')).tasks.find((task) => task.id === id);
    return task && terminal(task) ? task : null;
  }, '发送端收到最终落盘确认');
  assert.equal(sent.status, 'completed', JSON.stringify(sent));
  const received = (await peerCommand('snapshot')).tasks.find((task) => task.id === id);
  assert.equal(digest(await readFile(received.savedPath)), digest(content));
  await page.getByRole('button', { name: '传输记录', exact: true }).click();
  await expect(
    page
      .getByRole('listitem', { name: `任务 ${id}`, exact: true })
      .getByText('已完成', { exact: true }),
  ).toBeVisible();
  await page.screenshot({ path: path.join(output, 'outgoing.png'), fullPage: true });
  report.push('原生文件选择、真实 1 MiB 发送、对端落盘摘要及任务展示通过');

  const reverse = await peerCommand('send', {
    fingerprint: local.identity.fingerprint,
    path: source,
  });
  await until(
    async () =>
      (await invoke('get_transfer_snapshot')).tasks.some((task) => task.id === reverse.id),
    '原生应用收到接收请求',
  );
  const prompt = page.getByRole('region', { name: '接收请求 原生 sample.bin', exact: true });
  await prompt.getByRole('button', { name: '选择本次接收目录', exact: true }).click();
  execFileSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-File',
      path.join(root, 'scripts', 'native-file-dialog.ps1'),
      '-ApplicationPid',
      String(app.pid),
      '-FilePath',
      incoming,
      '-Directory',
    ],
    { stdio: 'inherit', timeout: 20_000 },
  );
  await expect(prompt.getByLabel('本次接收目录', { exact: true })).toHaveValue(incoming);
  await prompt.getByRole('button', { name: '接受文件', exact: true }).click();
  const incomingTask = await until(async () => {
    const task = (await invoke('get_transfer_snapshot')).tasks.find(
      (task) => task.id === reverse.id,
    );
    return task && terminal(task) ? task : null;
  }, '反向接收与落盘');
  assert.equal(incomingTask.status, 'completed', JSON.stringify(incomingTask));
  assert.equal(digest(await readFile(incomingTask.savedPath)), digest(content));
  await expect(
    page
      .getByRole('listitem', { name: `任务 ${reverse.id}`, exact: true })
      .getByText('已完成', { exact: true }),
  ).toBeVisible();
  await page.screenshot({ path: path.join(output, 'incoming.png'), fullPage: true });
  report.push('独立对端反向发送、原生接收授权及真实落盘通过');

  await until(
    async () =>
      terminal((await peerCommand('snapshot')).tasks.find((task) => task.id === reverse.id)),
    '对端发送任务结束',
  );
  const rejected = await peerCommand('send', {
    fingerprint: local.identity.fingerprint,
    path: source,
  });
  await prompt.getByRole('button', { name: '拒绝文件', exact: true }).click();
  await until(
    async () =>
      (await peerCommand('snapshot')).tasks.find((task) => task.id === rejected.id)?.status ===
      'rejected',
    '接收拒绝传回对端',
  );
  assert.equal(
    (await invoke('get_transfer_snapshot')).tasks.find((task) => task.id === rejected.id).savedPath,
    null,
  );
  report.push('真实接收拒绝操作未产生落盘成功记录');

  await revokeTestPeer();
  await peerCommand('forget', { fingerprint: local.identity.fingerprint });
  const connecting = peerCommand('connect', {
    port: local.service.port,
    fingerprint: local.identity.fingerprint,
  });
  const pairing = page.getByRole('region', { name: '配对请求 P2 原生验证对端', exact: true });
  await expect(pairing.getByRole('button', { name: '接受配对', exact: true })).toBeDisabled();
  await expect(pairing.getByText(peer.fingerprint, { exact: true })).toBeVisible();
  await pairing.getByLabel(verificationLabel, { exact: true }).check();
  await pairing.getByRole('button', { name: '接受配对', exact: true }).click();
  await connecting;
  await expect(page.getByRole('radio', { name: '选择接收设备 P2 原生验证对端' })).toBeVisible();
  report.push('真实撤销信任、对端重新发起配对和逐次完整指纹确认通过');

  const cancelled = await peerCommand('send', {
    fingerprint: local.identity.fingerprint,
    path: source,
  });
  await prompt.waitFor();
  closeWindow();
  confirmExit('继续传输');
  await until(
    async () =>
      (await invoke('get_transfer_snapshot')).tasks.some((task) => task.id === cancelled.id),
    '取消退出后恢复应用操作',
  );
  assert.equal(app.exitCode, null);
  await page.getByRole('button', { name: '传输记录', exact: true }).click();
  await page
    .getByRole('listitem', { name: `任务 ${cancelled.id}`, exact: true })
    .getByRole('button', { name: '取消传输', exact: true })
    .click();
  await until(
    async () =>
      (await peerCommand('snapshot')).tasks.find((task) => task.id === cancelled.id)?.status ===
      'cancelled',
    '界面取消传回对端',
  );
  report.push('有任务时取消退出保留窗口，界面取消任务传回对端');

  const stopped = await peerCommand('send', {
    fingerprint: local.identity.fingerprint,
    path: source,
  });
  await prompt.waitFor();
  closeWindow();
  confirmExit('停止任务并退出');
  await until(() => app.exitCode !== null, '停止任务后正常退出');
  await browser.close();
  browser = null;
  report.push('有待接收任务时确认退出完成协作清理');

  await launchApp();
  const restarted = await invoke('get_snapshot');
  assert.equal(restarted.identity.fingerprint, local.identity.fingerprint);
  assert.deepEqual(restarted.settings, local.settings);
  const persisted = await invoke('get_transfer_snapshot');
  assert(persisted.peers.some((item) => item.fingerprint === peer.fingerprint));
  assert.equal(persisted.tasks.length, 0);
  assert(!persisted.tasks.some((item) => item.id === stopped.id));
  await revokeTestPeer();
  assert.deepEqual(errors, []);
  report.push('重启保留真实身份和信任、无虚假恢复任务；已撤销测试信任且设置未变化');
  await peerCommand('stop');
  closeWindow();
  await until(() => app.exitCode !== null, '应用正常退出');
  await writeFile(
    path.join(output, 'result.json'),
    JSON.stringify(
      {
        report,
        profile,
        checkedAt: new Date().toISOString(),
        scope:
          'same-host native app and independent Rust identity; not dual-machine or macOS acceptance',
      },
      null,
      2,
    ),
  );
  console.log(report.join('\n'));
  console.log(`测试结果：${output}`);
} finally {
  if (page && peer && app?.exitCode === null) {
    try {
      await invoke('forget_peer', { fingerprint: peer.fingerprint });
    } catch {
      if (browser) await browser.close().catch(() => {});
      browser = null;
      if (app.exitCode === null) app.kill();
      await until(() => app.exitCode !== null || app.signalCode !== null, '失败测试进程退出');
      await launchApp();
      await invoke('forget_peer', { fingerprint: peer.fingerprint });
      closeWindow();
      await until(() => app.exitCode !== null, '测试信任清理后退出');
    }
  }
  if (browser) await browser.close().catch(() => {});
  if (app?.exitCode === null) app.kill();
  if (fixture.exitCode === null) {
    await peerCommand('stop').catch(() => {});
    await until(() => fixture.exitCode !== null, '测试对端退出', 10_000).catch(() =>
      fixture.kill(),
    );
  }
}
