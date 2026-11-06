import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const root = fileURLToPath(new URL('../', import.meta.url));
export function nativeEnv() {
  const env = { ...process.env };
  const cargoHome = path.join(root, '.tools', 'cargo');
  const binary = process.platform === 'win32' ? 'cargo.exe' : 'cargo';
  if (existsSync(path.join(cargoHome, 'bin', binary))) {
    const key = Object.keys(env).find((name) => name.toLowerCase() === 'path') ?? 'PATH';
    env[key] = `${path.join(cargoHome, 'bin')}${path.delimiter}${env[key] ?? ''}`;
    env.CARGO_HOME = cargoHome;
    env.RUSTUP_HOME = path.join(root, '.tools', 'rustup');
  }
  return env;
}
