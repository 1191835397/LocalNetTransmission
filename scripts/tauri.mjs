import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import path from 'node:path';
import { nativeEnv, root } from './native-env.mjs';

const require = createRequire(import.meta.url);
const cli = path.join(path.dirname(require.resolve('@tauri-apps/cli/package.json')), 'tauri.js');
const child = spawn(process.execPath, [cli, ...process.argv.slice(2)], {
  cwd: root,
  env: nativeEnv(),
  stdio: 'inherit',
});
child.on('error', (error) => {
  console.error(error.message);
  process.exitCode = 1;
});
child.on('exit', (code) => {
  process.exitCode = code ?? 1;
});
