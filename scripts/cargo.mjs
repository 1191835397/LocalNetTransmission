import { spawn } from 'node:child_process';
import { nativeEnv, root } from './native-env.mjs';

const child = spawn('cargo', process.argv.slice(2), {
  cwd: root,
  env: nativeEnv(),
  stdio: 'inherit',
});
child.on('error', (error) => {
  console.error(`无法启动 Cargo：${error.message}。请先运行 npm run doctor 检查工具链。`);
  process.exitCode = 1;
});
child.on('exit', (code) => {
  process.exitCode = code ?? 1;
});
