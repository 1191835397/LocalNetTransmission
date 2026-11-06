import { createHash } from 'node:crypto';
import { mkdir, writeFile, readFile } from 'node:fs/promises';
import { execFileSync, spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'win32' || process.arch !== 'x64') {
  throw new Error('此安装脚本仅支持 Windows x64，请在其他平台使用官方 Rust 工具链。');
}
const root = fileURLToPath(new URL('../', import.meta.url));
const tools = path.join(root, '.tools');
await mkdir(tools, { recursive: true });
const url = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe';
function download(address) {
  return execFileSync(
    'curl',
    [
      '--fail',
      '--location',
      '--proto',
      '=https',
      '--tlsv1.2',
      '--connect-timeout',
      '20',
      '--max-time',
      '180',
      address,
    ],
    {
      maxBuffer: 32 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'inherit'],
    },
  );
}
const checksum = (await download(`${url}.sha256`)).toString('utf8').trim().split(/\s+/)[0];
if (!/^[a-f0-9]{64}$/i.test(checksum)) throw new Error('官方校验值格式异常，终止安装。');
const binary = await download(url);
const actual = createHash('sha256').update(binary).digest('hex');
if (actual !== checksum.toLowerCase()) throw new Error('安装器 SHA-256 不匹配，终止安装。');
const installer = path.join(tools, 'rustup-init.exe');
await writeFile(installer, binary);
const persisted = createHash('sha256')
  .update(await readFile(installer))
  .digest('hex');
if (persisted !== actual) throw new Error('落盘校验失败，终止安装。');
console.log(`官方安装器 SHA-256 校验通过：${actual}`);
const child = spawn(
  installer,
  [
    '-y',
    '--no-modify-path',
    '--profile',
    'minimal',
    '--default-toolchain',
    'stable-x86_64-pc-windows-msvc',
    '--component',
    'rustfmt',
    '--component',
    'clippy',
  ],
  {
    stdio: 'inherit',
    env: {
      ...process.env,
      RUSTUP_HOME: path.join(tools, 'rustup'),
      CARGO_HOME: path.join(tools, 'cargo'),
    },
  },
);
child.on('error', (error) => {
  console.error(error.message);
  process.exitCode = 1;
});
child.on('exit', (code) => {
  process.exitCode = code ?? 1;
});
