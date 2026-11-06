import { describe, expect, it } from 'vitest';
import { errorMessage, formatBytes, mergeFiles, validateSettings } from '../src/utils';
import type { SelectedFile } from '../src/types';

const file = (id: string): SelectedFile => ({
  id,
  name: `${id}.txt`,
  size: 1024,
  modifiedAt: null,
});
const settings = { deviceName: '工作电脑', receiveDir: 'C:\\Users\\test\\Downloads', port: 53318 };

describe('formatBytes', () => {
  it.each([
    [0, '0 B'],
    [1, '1 B'],
    [1024, '1 KB'],
    [1536, '1.5 KB'],
    [1024 ** 3, '1 GB'],
    [10 * 1024 ** 3, '10 GB'],
  ])('formats %s', (input, expected) => {
    expect(formatBytes(input as number)).toBe(expected);
  });
});

describe('file selection', () => {
  it('deduplicates by file identity, not display name', () => {
    expect(mergeFiles([file('one')], [file('one'), file('two')])).toHaveLength(2);
    expect(
      mergeFiles(
        [],
        [
          { ...file('a'), name: 'same.txt' },
          { ...file('b'), name: 'same.txt' },
        ],
      ),
    ).toHaveLength(2);
  });
  it('rejects a combined selection over the limit without mutating existing selection', () => {
    const current = Array.from({ length: 128 }, (_, index) => file(String(index)));
    expect(() => mergeFiles(current, [file('extra')])).toThrow('128');
    expect(current).toHaveLength(128);
    expect(mergeFiles(current, [file('0')])).toHaveLength(128);
  });
});

describe('settings validation', () => {
  it('accepts valid settings', () => expect(validateSettings(settings)).toBeNull());
  it.each(['', '   ', 'x'.repeat(65), 'a\nb', 'a\u0000b'])(
    'rejects invalid name %j',
    (deviceName) => {
      expect(validateSettings({ ...settings, deviceName })).toContain('设备名称');
    },
  );
  it.each([0, 1023, 65536, 1.2, Number.NaN])('rejects invalid port %s', (port) => {
    expect(validateSettings({ ...settings, port })).toContain('端口');
  });
  it('requires a receive directory', () =>
    expect(validateSettings({ ...settings, receiveDir: '  ' })).toContain('接收目录'));
  it('supports 64 Unicode code points', () =>
    expect(validateSettings({ ...settings, deviceName: '台'.repeat(64) })).toBeNull());
});

it('normalizes native and JavaScript error messages', () => {
  expect(errorMessage(new Error('失败'))).toBe('失败');
  expect(errorMessage('原生错误')).toBe('原生错误');
});
