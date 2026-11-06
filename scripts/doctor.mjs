import { spawnSync } from 'node:child_process';
import { createServer, createConnection } from 'node:net';
import { nativeEnv } from './native-env.mjs';

const env = nativeEnv();
let failed = false;
function report(name, passed, detail) {
  console.log(`${passed ? 'PASS' : 'MISSING'}  ${name}  ${detail}`);
  if (!passed) failed = true;
}
report('Node.js', Number(process.versions.node.split('.')[0]) >= 22, process.version);
for (const command of ['rustc', 'cargo']) {
  const result = spawnSync(command, ['--version'], { env, encoding: 'utf8' });
  report(
    command,
    result.status === 0,
    result.stdout?.trim() || result.error?.message || result.stderr?.trim(),
  );
}
if (process.platform === 'win32') {
  const script =
    '$vs = "${env:ProgramFiles(x86)}\\Microsoft Visual Studio\\Installer\\vswhere.exe"; $install = if (Test-Path $vs) { & $vs -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath }; $wv = Get-ItemProperty "HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\*", "HKCU:\\SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\*" -ErrorAction SilentlyContinue | Where-Object { $_.name -like "*WebView2*" } | Select-Object -First 1; @{msvc=$install;webview=$wv.pv} | ConvertTo-Json -Compress';
  const result = spawnSync('powershell.exe', ['-NoProfile', '-Command', script], {
    encoding: 'utf8',
  });
  try {
    const data = JSON.parse(result.stdout);
    report('MSVC C++', Boolean(data.msvc), data.msvc || '未找到 C++ 工具链');
    report('WebView2', Boolean(data.webview), data.webview || '未找到运行时');
  } catch {
    report('Windows prerequisites', false, result.stderr || '无法读取环境检测结果');
  }
}
await new Promise((resolve) => {
  const server = createServer((socket) => socket.end('ok'));
  const timer = setTimeout(() => {
    server.close();
    report('Loopback TCP', false, '连接超时');
    resolve();
  }, 3000);
  server.once('error', (error) => {
    clearTimeout(timer);
    report('Loopback TCP', false, error.message);
    resolve();
  });
  server.listen(0, '127.0.0.1', () => {
    const client = createConnection(server.address().port, '127.0.0.1');
    client.once('data', () => {
      clearTimeout(timer);
      client.destroy();
      server.close();
      report('Loopback TCP', true, '本机收发正常（不代表局域网互通）');
      resolve();
    });
    client.once('error', (error) => {
      clearTimeout(timer);
      server.close();
      report('Loopback TCP', false, error.message);
      resolve();
    });
  });
});
console.log('INFO  Mac 真机、第二台 Windows、局域网互通与签名证书需另行验证。');
process.exitCode = failed ? 1 : 0;
