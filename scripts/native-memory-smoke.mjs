import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { nativeEnv, root } from './native-env.mjs';

if (process.platform !== 'win32') throw new Error('此内存采样脚本仅适用于 Windows。');
const output = await mkdtemp(path.join(root, '.tools', 'native-memory-'));
const artifacts = execFileSync(
  'cargo',
  ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--lib', '--no-run', '--message-format=json'],
  { cwd: root, env: nativeEnv(), encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, timeout: 600_000 },
)
  .trim()
  .split('\n')
  .map((line) => JSON.parse(line));
const executable = artifacts.find(
  (item) =>
    item.reason === 'compiler-artifact' &&
    item.profile.test &&
    item.target.name === 'lanlink_lib' &&
    item.executable,
)?.executable;
assert(executable, 'Cargo 未返回库测试可执行文件。');
const quote = (value) => `'${value.replaceAll("'", "''")}'`;
const results = [];
for (const [label, test, ignored] of [
  ['100-mib', 'engine::tests::real_tls_engine_empty_1k_and_100m_both_directions', false],
  ['1-gib-engine', 'engine::tests::real_tls_engine_one_gibibyte', true],
  ['1-gib-p2', 'p2_tests::one_gib_streaming_integrity', true],
]) {
  const stdout = path.join(output, `${label}.stdout.log`);
  const stderr = path.join(output, `${label}.stderr.log`);
  const script = `
$ErrorActionPreference = 'Stop'
$process = Start-Process -FilePath ${quote(executable)} -ArgumentList ${quote(`${test} --exact --nocapture --test-threads=1${ignored ? ' --ignored' : ''}`)} -WorkingDirectory ${quote(root)} -RedirectStandardOutput ${quote(stdout)} -RedirectStandardError ${quote(stderr)} -PassThru
$handle = $process.Handle
$peakWorkingSet = 0L
$peakPrivateBytes = 0L
$samples = 0
$watch = [Diagnostics.Stopwatch]::StartNew()
try {
    while (-not $process.HasExited) {
        $process.Refresh()
        if (-not $process.HasExited) {
            $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            $peakPrivateBytes = [Math]::Max($peakPrivateBytes, $process.PrivateMemorySize64)
            $samples++
        }
        if ($watch.Elapsed.TotalSeconds -gt 600) { throw 'Native memory test timed out.' }
        Start-Sleep -Milliseconds 50
    }
    $process.WaitForExit()
    @{ exitCode = $process.ExitCode; peakWorkingSetBytes = $peakWorkingSet; sampledPeakPrivateBytes = $peakPrivateBytes; samples = $samples; seconds = $watch.Elapsed.TotalSeconds } | ConvertTo-Json -Compress
} finally {
    if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
    $process.Dispose()
}
`;
  const measured = JSON.parse(
    execFileSync(
      'powershell.exe',
      ['-NoProfile', '-EncodedCommand', Buffer.from(script, 'utf16le').toString('base64')],
      {
        cwd: root,
        env: nativeEnv(),
        encoding: 'utf8',
        timeout: 620_000,
      },
    ).trim(),
  );
  const log = await readFile(stdout, 'utf8');
  console.log(log);
  const result = { label, test, ...measured };
  results.push(result);
  await writeFile(
    path.join(output, 'result.json'),
    JSON.stringify(
      {
        checkedAt: new Date().toISOString(),
        scope:
          'Same-host debug Rust test process including both TLS identities and disk verification; excludes WebView2 and is not dual-machine acceptance.',
        sampleIntervalMs: 50,
        results,
      },
      null,
      2,
    ),
  );
  assert.equal(measured.exitCode, 0, await readFile(stderr, 'utf8'));
  assert.match(log, /1 passed; 0 failed; 0 ignored/);
  assert(measured.samples > 0 && measured.peakWorkingSetBytes > 0);
  console.log(
    `${label}: peak working set ${(measured.peakWorkingSetBytes / 1024 ** 2).toFixed(1)} MiB, sampled private ${(measured.sampledPeakPrivateBytes / 1024 ** 2).toFixed(1)} MiB`,
  );
}
assert(
  results[1].peakWorkingSetBytes <= results[0].peakWorkingSetBytes + 64 * 1024 ** 2,
  '1 GiB 用例工作集比 100 MiB 基线增长超过 64 MiB。',
);
assert(
  results[1].sampledPeakPrivateBytes <= results[0].sampledPeakPrivateBytes + 64 * 1024 ** 2,
  '1 GiB 用例私有内存比 100 MiB 基线增长超过 64 MiB。',
);
console.log(`测试结果：${output}`);
