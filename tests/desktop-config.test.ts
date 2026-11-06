import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import manifest from '../package.json';
import config from '../src-tauri/tauri.conf.json';
import capability from '../src-tauri/capabilities/main.json';

const cargo = readFileSync('src-tauri/Cargo.toml', 'utf8');
function nativeVersion(name: string) {
  const line = cargo.split('\n').find((entry) => entry.startsWith(`${name} = `));
  const match = line?.match(/"=(\d+)\.(\d+)\.(\d+)"/);
  if (!match) throw new Error(`未找到锁定的原生依赖版本：${name}`);
  return `${match[1]}.${match[2]}`;
}

describe('desktop integration invariants', () => {
  it('keeps Tauri API versions on matching major/minor releases', () => {
    expect(manifest.dependencies['@tauri-apps/api'].split('.').slice(0, 2).join('.')).toBe(
      nativeVersion('tauri'),
    );
    expect(
      manifest.dependencies['@tauri-apps/plugin-dialog'].split('.').slice(0, 2).join('.'),
    ).toBe(nativeVersion('tauri-plugin-dialog'));
  });
  it('keeps app and frontend release versions consistent', () => {
    expect(config.version).toBe(manifest.version);
    expect(cargo).toContain(`\nversion = "${manifest.version}"`);
  });
  it('exposes no shell, arbitrary filesystem or remote window capabilities', () => {
    expect(capability.windows).toEqual(['main']);
    expect(capability.permissions).toEqual(['core:default', 'dialog:allow-open']);
    expect('remote' in capability).toBe(false);
    expect(config.app.security.capabilities).toEqual(['main']);
  });
  it('does not relax production script policy or expose development endpoints', () => {
    const directives = config.app.security.csp.split(';').map((entry) => entry.trim());
    expect(directives).toContain("script-src 'self'");
    expect(directives).toContain("object-src 'none'");
    expect(directives).toContain("frame-src 'none'");
    expect(config.app.security.csp).not.toMatch(/unsafe-eval|\*|ws:|1420/);
    expect(config.app.windows[0].create).toBe(false);
  });
});
