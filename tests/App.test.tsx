import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import App from '../src/App';
import * as bridge from '../src/bridge';
import { validFingerprint, validPeerAddress, verificationLabel } from '../src/TransferUI';
import type { AppSnapshot, PeerInfo, TransferInfo, TransferSnapshot } from '../src/types';

vi.mock('../src/bridge', () => ({
  isDesktop: vi.fn(() => false),
  getSnapshot: vi.fn(),
  saveSettings: vi.fn(),
  inspectFiles: vi.fn(),
  chooseFiles: vi.fn(),
  chooseDirectory: vi.fn(),
  runDiagnostics: vi.fn(),
  getTransferSnapshot: vi.fn(),
  connectPeer: vi.fn(),
  respondPair: vi.fn(),
  forgetPeer: vi.fn(),
  sendFile: vi.fn(),
  respondTransfer: vi.fn(),
  cancelTransfer: vi.fn(),
  refreshTransferResult: vi.fn(),
}));
vi.mock('@tauri-apps/api/webview', () => ({
  getCurrentWebview: vi.fn(),
}));

const snapshot: AppSnapshot = {
  version: '0.1.0',
  device: {
    id: 'test-device',
    name: '测试电脑',
    platform: 'windows',
    arch: 'x86_64',
    addresses: ['192.168.1.2'],
  },
  settings: { deviceName: '测试电脑', receiveDir: 'C:\\Downloads', port: 53318 },
  identity: {
    fingerprint: Array(32).fill('AA').join(':'),
    createdAt: '2026-10-30T09:00:00Z',
    storage: 'Windows Credential Manager',
  },
  service: { state: 'running', address: '0.0.0.0', port: 53318, error: null, scope: 'lan' },
};
const peer: PeerInfo = {
  fingerprint: Array(32).fill('BB').join(':'),
  name: '接收电脑',
  address: '192.168.1.3',
  port: 53318,
};
const emptyTransfers: TransferSnapshot = { peers: [], pairRequests: [], tasks: [] };
const nativeFile = { name: 'data.bin', path: 'C:\\data.bin', size: 1024, modifiedAt: null };
function task(overrides: Partial<TransferInfo> = {}): TransferInfo {
  return {
    id: 'task-1',
    direction: 'send',
    peerFingerprint: peer.fingerprint,
    peerName: peer.name,
    fileName: 'data.bin',
    size: 1024,
    bytesTransferred: 0,
    bytesPerSecond: 0,
    status: 'awaitingAcceptance',
    error: null,
    savedPath: null,
    ...overrides,
  };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(getCurrentWebview).mockReturnValue({
    onDragDropEvent: async () => () => {},
  } as unknown as ReturnType<typeof getCurrentWebview>);
  vi.mocked(bridge.isDesktop).mockReturnValue(false);
  vi.mocked(bridge.getSnapshot).mockResolvedValue(structuredClone(snapshot));
  vi.mocked(bridge.getTransferSnapshot).mockResolvedValue(structuredClone(emptyTransfers));
  vi.mocked(bridge.chooseFiles).mockResolvedValue([nativeFile]);
  vi.mocked(bridge.respondPair).mockResolvedValue(undefined);
  vi.mocked(bridge.respondTransfer).mockResolvedValue(undefined);
  vi.mocked(bridge.forgetPeer).mockResolvedValue(undefined);
  vi.mocked(bridge.cancelTransfer).mockResolvedValue(undefined);
  vi.mocked(bridge.refreshTransferResult).mockResolvedValue(undefined);
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

async function go(label: string) {
  await userEvent.click(screen.getByRole('button', { name: label }));
}

describe('browser preview', () => {
  it('does not call native APIs or invent nearby devices', () => {
    render(<App />);
    expect(screen.getByText('浏览器预览 · 无原生连接')).toBeInTheDocument();
    expect(screen.getByText('这里没有模拟设备。', { exact: false })).toBeInTheDocument();
    expect(bridge.getSnapshot).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
  });
  it('allows choosing, deduplicating and removing real local preview files without sending', async () => {
    render(<App />);
    const file = new File(['hello'], '计划.txt', { type: 'text/plain', lastModified: 1000 });
    const input = screen.getByLabelText('浏览器预览选择文件');
    fireEvent.change(input, { target: { files: [file, file] } });
    expect(screen.getByText('计划.txt')).toBeInTheDocument();
    expect(screen.getAllByRole('button', { name: '移除 计划.txt' })).toHaveLength(1);
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    await go('移除 计划.txt');
    expect(screen.queryByText('计划.txt')).not.toBeInTheDocument();
  });
  it('rejects excess files without losing the current selection', () => {
    render(<App />);
    const input = screen.getByLabelText('浏览器预览选择文件');
    fireEvent.change(input, { target: { files: [new File(['a'], 'keep.txt')] } });
    fireEvent.change(input, {
      target: { files: Array.from({ length: 128 }, (_, index) => new File(['b'], `${index}.txt`)) },
    });
    expect(screen.getByRole('alert')).toHaveTextContent('最多可选择 128');
    expect(screen.getByText('keep.txt')).toBeInTheDocument();
  });
  it('does not allow saving native settings from preview', async () => {
    render(<App />);
    await go('偏好设置');
    expect(screen.getByLabelText('设备名称')).toBeDisabled();
    expect(screen.getByRole('button', { name: '保存设置' })).toBeDisabled();
    expect(bridge.saveSettings).not.toHaveBeenCalled();
  });
  it('switches tabs without inventing transfer history', async () => {
    render(<App />);
    await go('传输记录');
    expect(screen.getByText('还没有传输任务')).toBeInTheDocument();
    await go('先准备文件');
    expect(screen.getByText('把文件拖到这里')).toBeInTheDocument();
    await go('设备与安全');
    expect(screen.getByRole('button', { name: '运行检查' })).toBeDisabled();
  });
});

describe('desktop IPC integration', () => {
  beforeEach(() => vi.mocked(bridge.isDesktop).mockReturnValue(true));
  it('renders real snapshot fields and does not imply LAN discovery is running', async () => {
    render(<App />);
    expect(await screen.findByText('测试电脑')).toBeInTheDocument();
    expect(screen.getByText('192.168.1.2')).toBeInTheDocument();
    expect(screen.getByText('本机 TLS 服务已启动')).toBeInTheDocument();
    expect(screen.getByText('自动发现尚未启用')).toBeInTheDocument();
  });
  it('explains restart after initialization failure and still allows diagnostics', async () => {
    vi.mocked(bridge.getSnapshot).mockRejectedValueOnce('身份存储不可用');
    vi.mocked(bridge.runDiagnostics).mockResolvedValue([
      { name: '系统身份存储', status: 'fail', detail: '请解锁系统凭据并重启应用' },
    ]);
    render(<App />);
    expect(await screen.findByRole('alert')).toHaveTextContent('身份存储不可用');
    expect(
      screen.getByText('本机初始化未完成。请处理错误后退出并重新打开应用。'),
    ).toBeInTheDocument();
    await go('查看诊断');
    await go('运行检查');
    expect(await screen.findByText('请解锁系统凭据并重启应用')).toBeInTheDocument();
    expect(bridge.getSnapshot).toHaveBeenCalledOnce();
  });
  it('chooses native files and reports a rejected directory', async () => {
    vi.mocked(bridge.chooseFiles)
      .mockResolvedValueOnce([
        { name: 'data.bin', path: 'C:\\data.bin', size: 0, modifiedAt: null },
      ])
      .mockRejectedValueOnce('当前不支持文件夹');
    render(<App />);
    await screen.findByText('测试电脑');
    await go('选择文件');
    expect(await screen.findByText('data.bin')).toBeInTheDocument();
    await go('选择文件');
    expect(await screen.findByRole('alert')).toHaveTextContent('不支持文件夹');
    expect(screen.getByText('data.bin')).toBeInTheDocument();
  });
  it('validates settings, persists trimmed name and reports restart requirement', async () => {
    vi.mocked(bridge.saveSettings).mockImplementation(async (settings) => ({
      ...snapshot,
      settings,
      device: { ...snapshot.device, name: settings.deviceName },
    }));
    render(<App />);
    await screen.findByText('测试电脑');
    await go('偏好设置');
    fireEvent.change(screen.getByLabelText('设备名称'), { target: { value: '  ' } });
    await go('保存设置');
    expect(screen.getByRole('alert')).toHaveTextContent('设备名称');
    expect(bridge.saveSettings).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('设备名称'), { target: { value: ' 新电脑 ' } });
    fireEvent.change(screen.getByLabelText('本机服务端口'), { target: { value: '53319' } });
    await go('保存设置');
    await waitFor(() =>
      expect(bridge.saveSettings).toHaveBeenCalledWith({
        deviceName: '新电脑',
        receiveDir: 'C:\\Downloads',
        port: 53319,
      }),
    );
    expect(await screen.findByRole('status')).toHaveTextContent('重启应用后生效');
  });
  it('does not pretend to save when the backend rejects the write', async () => {
    vi.mocked(bridge.saveSettings).mockRejectedValue('配置目录无写入权限');
    render(<App />);
    await screen.findByText('测试电脑');
    await go('偏好设置');
    await go('保存设置');
    expect(await screen.findByRole('alert')).toHaveTextContent('无写入权限');
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
  it('renders actual failing diagnostic output', async () => {
    vi.mocked(bridge.runDiagnostics).mockResolvedValue([
      { name: 'TLS', status: 'fail', detail: '证书校验失败' },
    ]);
    render(<App />);
    await screen.findByText('测试电脑');
    await go('设备与安全');
    await go('运行检查');
    expect(await screen.findByText('证书校验失败')).toBeInTheDocument();
    expect(bridge.runDiagnostics).toHaveBeenCalledOnce();
  });
});

async function fillConnection() {
  fireEvent.change(screen.getByLabelText('对方 IPv4 地址'), { target: { value: peer.address } });
  fireEvent.change(screen.getByLabelText('对方完整 SHA-256 指纹'), {
    target: { value: peer.fingerprint },
  });
}
async function advancePoll() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(750);
  });
}
async function renderNative(value: TransferSnapshot = { ...emptyTransfers, peers: [peer] }) {
  vi.mocked(bridge.isDesktop).mockReturnValue(true);
  vi.mocked(bridge.getTransferSnapshot).mockResolvedValue(value);
  const view = render(<App />);
  await act(async () => {});
  return view;
}

describe('P2 native pairing and transfers', () => {
  it('requires a full verified fingerprint and waits for remote approval without inventing peers', async () => {
    const request = deferred<PeerInfo>();
    vi.mocked(bridge.connectPeer).mockReturnValue(request.promise);
    await renderNative(emptyTransfers);
    await fillConnection();
    expect(screen.getByRole('button', { name: '请求配对' })).toBeDisabled();
    await userEvent.click(screen.getByLabelText(verificationLabel));
    await go('请求配对');
    expect(bridge.connectPeer).toHaveBeenCalledWith(peer.address, 53318, peer.fingerprint);
    expect(screen.getByText(/正在等待对方响应并批准配对/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '等待对方配对响应…' })).toBeDisabled();
    expect(screen.queryByRole('radio')).not.toBeInTheDocument();
    await go('设备与安全');
    expect(screen.getByText(snapshot.identity.fingerprint)).toBeInTheDocument();
    await go('发送文件');
    expect(screen.getByRole('button', { name: '等待对方配对响应…' })).toBeDisabled();
    await act(async () => request.resolve(peer));
    expect(screen.getByRole('status')).toHaveTextContent('对方已批准配对');
    expect(screen.queryByRole('radio')).not.toBeInTheDocument();
    expect(bridge.connectPeer).toHaveBeenCalledOnce();
  });

  it.each(['8.8.8.8', '0.0.0.0', '192.168.1.256', 'localhost', '192.168.01.1', '::1'])(
    'blocks invalid/public address %s',
    async (address) => {
      await renderNative();
      await fillConnection();
      fireEvent.change(screen.getByLabelText('对方 IPv4 地址'), { target: { value: address } });
      await userEvent.click(screen.getByLabelText(verificationLabel));
      expect(screen.getByText('请输入有效的局域网 IPv4 地址。')).toBeInTheDocument();
      expect(screen.getByRole('button', { name: '请求配对' })).toBeDisabled();
      expect(bridge.connectPeer).not.toHaveBeenCalled();
    },
  );

  it('invalidates verification when identity changes and rejects short codes and invalid ports', async () => {
    await renderNative();
    await fillConnection();
    await userEvent.click(screen.getByLabelText(verificationLabel));
    expect(screen.getByRole('button', { name: '请求配对' })).toBeEnabled();
    fireEvent.change(screen.getByLabelText('对方完整 SHA-256 指纹'), {
      target: { value: '123456' },
    });
    expect(screen.getByLabelText(verificationLabel)).not.toBeChecked();
    expect(screen.getByText(/短码不能用于配对/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '请求配对' })).toBeDisabled();
    await fillConnection();
    for (const port of ['1023', '65536', '53318.5', '1e4', '']) {
      fireEvent.change(screen.getByLabelText('对方端口'), { target: { value: port } });
      expect(screen.getByRole('button', { name: '请求配对' })).toBeDisabled();
    }
    expect(bridge.connectPeer).not.toHaveBeenCalled();
  });

  it('surfaces pairing rejection and allows an explicit retry', async () => {
    vi.mocked(bridge.connectPeer).mockRejectedValue('对方拒绝了配对请求');
    await renderNative();
    await fillConnection();
    await userEvent.click(screen.getByLabelText(verificationLabel));
    await go('请求配对');
    expect(screen.getByRole('alert')).toHaveTextContent('对方拒绝了配对请求');
    expect(screen.getByRole('button', { name: '请求配对' })).toBeEnabled();
  });

  it('shows global caller identity and independent per-request verification, including rejection without verification', async () => {
    await renderNative({
      ...emptyTransfers,
      pairRequests: [
        { ...peer, id: 'pair-1', expiresAt: '2099-01-01T00:00:00Z' },
        {
          ...peer,
          id: 'pair-2',
          name: '<b>another peer</b>',
          fingerprint: 'cc'.repeat(32),
          expiresAt: '2099-01-01T00:00:00Z',
        },
      ],
    });
    await fillConnection();
    const manual = within(screen.getByRole('region', { name: '手动连接设备' }));
    await userEvent.click(manual.getByLabelText(verificationLabel));
    await go('偏好设置');
    const first = within(screen.getByRole('region', { name: `配对请求 ${peer.name}` }));
    const second = within(screen.getByRole('region', { name: '配对请求 <b>another peer</b>' }));
    expect(first.getByText(peer.fingerprint)).toBeInTheDocument();
    expect(first.getByText(`${peer.address}:${peer.port}`)).toBeInTheDocument();
    expect(first.getByRole('button', { name: '接受配对' })).toBeDisabled();
    expect(bridge.respondPair).not.toHaveBeenCalled();
    await userEvent.click(first.getByLabelText(verificationLabel));
    expect(second.getByRole('button', { name: '接受配对' })).toBeDisabled();
    await userEvent.click(first.getByRole('button', { name: '接受配对' }));
    expect(bridge.respondPair).toHaveBeenCalledWith('pair-1', true);
    expect(first.getByRole('button', { name: '接受配对' })).toBeDisabled();
    await userEvent.click(second.getByRole('button', { name: '拒绝配对' }));
    expect(bridge.respondPair).toHaveBeenCalledWith('pair-2', false);
    expect(second.getByText('<b>another peer</b>').querySelector('b')).toBeNull();
  });

  it('disables expired approvals and reports response failures', async () => {
    await renderNative({
      ...emptyTransfers,
      pairRequests: [{ ...peer, id: 'expired', expiresAt: '2000-01-01T00:00:00Z' }],
    });
    expect(screen.getByRole('button', { name: '接受配对' })).toBeDisabled();
    vi.mocked(bridge.respondPair).mockRejectedValue('请求已过期');
    await go('拒绝配对');
    expect(screen.getByRole('alert')).toHaveTextContent('请求已过期');
  });

  it('requires a selected trusted device and exactly one native file, then uses the returned task ID', async () => {
    vi.mocked(bridge.chooseFiles).mockResolvedValueOnce([
      nativeFile,
      { ...nativeFile, path: 'C:\\second.bin', name: 'second.bin' },
    ]);
    vi.mocked(bridge.sendFile).mockResolvedValue('real-task-id');
    await renderNative();
    await go('选择文件');
    await userEvent.click(screen.getByRole('radio', { name: `选择接收设备 ${peer.name}` }));
    expect(screen.getByText(/不会自动发送第一个/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    expect(bridge.sendFile).not.toHaveBeenCalled();
    await go('移除 second.bin');
    await go('发送所选文件');
    expect(bridge.sendFile).toHaveBeenCalledWith(peer.fingerprint, nativeFile.path);
    expect(screen.getByRole('heading', { name: '传输记录' })).toBeInTheDocument();
    expect(screen.getByText(/已创建发送任务：real-task-id/)).toBeInTheDocument();
    expect(screen.queryByRole('progressbar')).not.toBeInTheDocument();
    await go('发送文件');
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
  });

  it('only enables send with a running engine, trusted selection and native path', async () => {
    await renderNative();
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    await go('选择文件');
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    await userEvent.click(screen.getByRole('radio'));
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeEnabled();
    await go('清空');
    fireEvent.change(screen.getByLabelText('浏览器预览选择文件'), {
      target: { files: [new File(['hi'], 'no-path.txt')] },
    });
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    cleanup();
    vi.mocked(bridge.getSnapshot).mockResolvedValue({
      ...snapshot,
      service: { ...snapshot.service, state: 'error', error: 'bind failed' },
    });
    await renderNative();
    await go('选择文件');
    expect(screen.getByRole('radio')).toBeDisabled();
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
  });

  it('blocks new sends while a previous receive result is unknown', async () => {
    await renderNative({ ...emptyTransfers, peers: [peer], tasks: [task({ status: 'unknown' })] });
    await go('选择文件');
    await userEvent.click(screen.getByRole('radio'));
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    expect(bridge.sendFile).not.toHaveBeenCalled();
  });

  it('keeps send errors visible without creating fake tasks', async () => {
    vi.mocked(bridge.sendFile).mockRejectedValue('连接被拒绝');
    await renderNative();
    await go('选择文件');
    await userEvent.click(screen.getByRole('radio'));
    await go('发送所选文件');
    expect(screen.getByRole('alert')).toHaveTextContent('连接被拒绝');
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeEnabled();
    await go('传输记录');
    expect(screen.getByText('还没有传输任务')).toBeInTheDocument();
  });

  it('revokes trust explicitly and clears the recipient without claiming offline status', async () => {
    await renderNative();
    await go('选择文件');
    await userEvent.click(screen.getByRole('radio'));
    await go(`撤销信任 ${peer.name}`);
    expect(bridge.forgetPeer).not.toHaveBeenCalled();
    await go('确认撤销信任');
    expect(bridge.forgetPeer).toHaveBeenCalledWith(peer.fingerprint);
    expect(screen.getByRole('radio')).not.toBeChecked();
    expect(screen.getByRole('radio')).toBeDisabled();
    expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
    expect(screen.queryByText('离线')).not.toBeInTheDocument();
  });

  it('shows incoming metadata globally, uses the saved default rather than draft, and requires acceptance', async () => {
    const incoming = task({ direction: 'receive', fileName: '<img src=x onerror=alert(1)>.txt' });
    await renderNative({ ...emptyTransfers, tasks: [incoming] });
    await go('偏好设置');
    vi.mocked(bridge.chooseDirectory).mockResolvedValueOnce('C:\\Unsaved');
    await go('选择目录');
    const panel = within(screen.getByRole('region', { name: `接收请求 ${incoming.fileName}` }));
    expect(panel.getByText(peer.name)).toBeInTheDocument();
    expect(panel.getByText(peer.fingerprint)).toBeInTheDocument();
    expect(panel.getByText(incoming.fileName).querySelector('img')).toBeNull();
    expect(panel.getByLabelText('本次接收目录（默认目录）')).toHaveValue('C:\\Downloads');
    expect(panel.getByLabelText('本次接收目录（默认目录）')).toHaveAttribute('readonly');
    expect(bridge.respondTransfer).not.toHaveBeenCalled();
    await userEvent.click(panel.getByRole('button', { name: '接受文件' }));
    expect(bridge.respondTransfer).toHaveBeenCalledWith(incoming.id, true, 'C:\\Downloads');
    expect(panel.getByRole('button', { name: '接受文件' })).toBeDisabled();
  });

  it('chooses a native receive directory and rejects independently without a directory', async () => {
    await renderNative({
      ...emptyTransfers,
      tasks: [
        task({ direction: 'receive' }),
        task({ id: 'decline', direction: 'receive', fileName: 'decline.bin' }),
      ],
    });
    vi.mocked(bridge.chooseDirectory).mockResolvedValue('C:\\Incoming');
    const first = within(screen.getByRole('region', { name: '接收请求 data.bin' }));
    const second = within(screen.getByRole('region', { name: '接收请求 decline.bin' }));
    await userEvent.click(first.getByRole('button', { name: '选择本次接收目录' }));
    expect(bridge.chooseDirectory).toHaveBeenCalledWith('C:\\Downloads');
    expect(first.getByLabelText('本次接收目录')).toHaveValue('C:\\Incoming');
    await userEvent.click(first.getByRole('button', { name: '接受文件' }));
    await userEvent.click(second.getByRole('button', { name: '拒绝文件' }));
    expect(bridge.respondTransfer).toHaveBeenCalledWith('task-1', true, 'C:\\Incoming');
    expect(bridge.respondTransfer).toHaveBeenCalledWith('decline', false, null);
  });

  it('handles receive chooser cancellation and backend failures without reporting success', async () => {
    await renderNative({ ...emptyTransfers, tasks: [task({ direction: 'receive' })] });
    vi.mocked(bridge.chooseDirectory).mockResolvedValue(null);
    await go('选择本次接收目录');
    expect(screen.getByLabelText('本次接收目录（默认目录）')).toHaveValue('C:\\Downloads');
    vi.mocked(bridge.respondTransfer).mockRejectedValue('目录不可写');
    await go('接受文件');
    expect(screen.getByRole('alert')).toHaveTextContent('目录不可写');
    expect(screen.getByRole('button', { name: '接受文件' })).toBeEnabled();
    expect(screen.queryByText(/接收响应已提交/)).not.toBeInTheDocument();
  });

  it('renders real task phases and bytes; 100% is not completion; cancellation and unknown query use IDs', async () => {
    await renderNative({
      ...emptyTransfers,
      tasks: [
        task({ id: 'hash', status: 'hashing' }),
        task({ id: 'wait' }),
        task({ id: 'ready', status: 'ready' }),
        task({ id: 'active', status: 'transferring', bytesTransferred: 512, bytesPerSecond: 128 }),
        task({
          id: 'verify',
          status: 'verifying',
          bytesTransferred: 1024,
          savedPath: 'C:\\hidden.bin',
        }),
        task({
          id: 'done',
          direction: 'receive',
          status: 'completed',
          bytesTransferred: 1024,
          savedPath: 'C:\\done.bin',
        }),
        task({ id: 'sent', status: 'completed', savedPath: 'C:\\remote.bin' }),
        task({ id: 'rejected', status: 'rejected' }),
        task({ id: 'cancelled', status: 'cancelled' }),
        task({ id: 'failed', status: 'failed', error: '<b>网络断开</b>' }),
        task({ id: 'unknown', status: 'unknown', bytesTransferred: 1024 }),
      ],
    });
    await go('传输记录');
    for (const label of [
      '正在计算文件哈希',
      '等待接收方确认',
      '已就绪，等待传输',
      '正在传输',
      '正在校验',
      '已拒绝',
      '已取消',
      '传输失败',
      '接收结果未知',
    ])
      expect(screen.getByText(label)).toBeInTheDocument();
    const active = within(screen.getByRole('listitem', { name: '任务 active' }));
    expect(active.getByRole('progressbar')).toHaveAttribute('value', '50');
    expect(active.getByText('512 B / 1 KB · 50.0%')).toBeInTheDocument();
    expect(active.getByText('128 B / 秒')).toBeInTheDocument();
    const verify = within(screen.getByRole('listitem', { name: '任务 verify' }));
    expect(verify.getByRole('progressbar')).toHaveAttribute('value', '100');
    expect(verify.queryByText('已完成')).not.toBeInTheDocument();
    expect(verify.getByText(/不代表成功/)).toBeInTheDocument();
    expect(screen.getByText('已保存到：C:\\done.bin')).toBeInTheDocument();
    expect(screen.queryByText(/hidden.bin|remote.bin/)).not.toBeInTheDocument();
    expect(screen.getByText('<b>网络断开</b>').querySelector('b')).toBeNull();
    await userEvent.click(active.getByRole('button', { name: '取消传输' }));
    expect(bridge.cancelTransfer).toHaveBeenCalledWith('active');
    await go('查询接收结果');
    expect(bridge.refreshTransferResult).toHaveBeenCalledWith('unknown');
    expect(bridge.sendFile).not.toHaveBeenCalled();
    expect(screen.queryByRole('button', { name: /重发|重新发送/ })).not.toBeInTheDocument();
  });

  it('follows the native success flow without optimistic progress and blocks concurrent sends', async () => {
    vi.useFakeTimers();
    let current: TransferSnapshot = { ...emptyTransfers, peers: [peer] };
    await renderNative(current);
    vi.mocked(bridge.getTransferSnapshot).mockImplementation(async () => current);
    vi.mocked(bridge.sendFile).mockResolvedValue('task-1');
    await act(async () => fireEvent.click(screen.getByRole('button', { name: '选择文件' })));
    fireEvent.click(screen.getByRole('radio'));
    await act(async () => fireEvent.click(screen.getByRole('button', { name: '发送所选文件' })));
    expect(screen.queryByRole('progressbar')).not.toBeInTheDocument();
    for (const status of [
      'hashing',
      'awaitingAcceptance',
      'transferring',
      'verifying',
      'completed',
    ] as const) {
      current = {
        ...current,
        tasks: [
          task({
            status,
            bytesTransferred: status === 'verifying' || status === 'completed' ? 1024 : 0,
          }),
        ],
      };
      await advancePoll();
      fireEvent.click(screen.getByRole('button', { name: '发送文件' }));
      if (status !== 'completed')
        expect(screen.getByRole('button', { name: '发送所选文件' })).toBeDisabled();
      else expect(screen.getByRole('button', { name: '发送所选文件' })).toBeEnabled();
      fireEvent.click(screen.getByRole('button', { name: '传输记录' }));
    }
    expect(screen.getByText('已完成')).toBeInTheDocument();
    expect(bridge.sendFile).toHaveBeenCalledOnce();
  });

  it('surfaces cancel and result-query errors without changing task state', async () => {
    await renderNative({
      ...emptyTransfers,
      tasks: [task({ status: 'transferring' }), task({ id: 'unknown', status: 'unknown' })],
    });
    vi.mocked(bridge.cancelTransfer).mockRejectedValue('取消失败');
    vi.mocked(bridge.refreshTransferResult).mockRejectedValue('查询超时');
    await go('传输记录');
    await go('取消传输');
    expect(screen.getByRole('alert')).toHaveTextContent('取消失败');
    expect(screen.getByText('正在传输')).toBeInTheDocument();
    await go('查询接收结果');
    expect(screen.getByRole('alert')).toHaveTextContent('查询超时');
    expect(screen.getByText('接收结果未知')).toBeInTheDocument();
  });
});

describe('P2 snapshot lifecycle', () => {
  it('does not poll or call any native API in browser preview after navigation and file selection', async () => {
    vi.useFakeTimers();
    render(<App />);
    fireEvent.change(screen.getByLabelText('浏览器预览选择文件'), {
      target: { files: [new File(['a'], 'a.txt')] },
    });
    for (const label of ['传输记录', '设备与安全', '偏好设置', '发送文件'])
      fireEvent.click(screen.getByRole('button', { name: label }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    for (const [name, method] of Object.entries(bridge))
      if (name !== 'isDesktop') expect(method).not.toHaveBeenCalled();
    expect(getCurrentWebview).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: '请求配对' })).toBeDisabled();
    expect(screen.queryByRole('radio')).not.toBeInTheDocument();
  });

  it('does not poll until the initial snapshot succeeds, and never polls after initialization failure', async () => {
    vi.useFakeTimers();
    const initial = deferred<AppSnapshot>();
    vi.mocked(bridge.getSnapshot).mockReturnValue(initial.promise);
    await renderNative();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(bridge.getTransferSnapshot).not.toHaveBeenCalled();
    await act(async () => initial.reject('backend missing'));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(bridge.getTransferSnapshot).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent('backend missing');
  });

  it('polls serially, preserves settings drafts, and cancels timers and late results on unmount', async () => {
    vi.useFakeTimers();
    const first = deferred<TransferSnapshot>();
    vi.mocked(bridge.isDesktop).mockReturnValue(true);
    vi.mocked(bridge.getTransferSnapshot).mockReturnValueOnce(first.promise);
    const view = render(<App />);
    await act(async () => {});
    expect(bridge.getTransferSnapshot).toHaveBeenCalledOnce();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(bridge.getTransferSnapshot).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole('button', { name: '偏好设置' }));
    fireEvent.change(screen.getByLabelText('设备名称'), { target: { value: '未保存的名称' } });
    await act(async () => first.resolve(emptyTransfers));
    await advancePoll();
    expect(bridge.getTransferSnapshot).toHaveBeenCalledTimes(2);
    expect(bridge.getSnapshot).toHaveBeenCalledOnce();
    expect(screen.getByLabelText('设备名称')).toHaveValue('未保存的名称');
    const late = deferred<TransferSnapshot>();
    vi.mocked(bridge.getTransferSnapshot).mockReturnValueOnce(late.promise);
    await advancePoll();
    view.unmount();
    await act(async () => late.resolve(emptyTransfers));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(bridge.getTransferSnapshot).toHaveBeenCalledTimes(3);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('surfaces poll errors once, honors dismissal, and clears stale status on recovery', async () => {
    vi.useFakeTimers();
    await renderNative();
    vi.mocked(bridge.getTransferSnapshot).mockRejectedValue('temporary outage');
    await advancePoll();
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    expect(screen.getByRole('alert')).toHaveTextContent('temporary outage');
    fireEvent.click(screen.getByRole('button', { name: '关闭状态更新错误' }));
    await advancePoll();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByText(/状态更新中断/)).toBeInTheDocument();
    vi.mocked(bridge.getTransferSnapshot).mockResolvedValue(emptyTransfers);
    await advancePoll();
    expect(screen.queryByText(/状态更新中断/)).not.toBeInTheDocument();
    vi.mocked(bridge.getTransferSnapshot).mockRejectedValue('temporary outage');
    await advancePoll();
    expect(screen.getAllByRole('alert')).toHaveLength(1);
  });
});

describe('pairing validators', () => {
  it.each(['10.0.0.1', '172.16.0.1', '172.31.255.254', '192.168.1.1', '127.0.0.1', '169.254.1.2'])(
    'allows local IPv4 %s',
    (address) => {
      expect(validPeerAddress(address)).toBe(true);
    },
  );
  it.each([
    '172.15.0.1',
    '172.32.0.1',
    '192.0.2.1',
    '255.255.255.255',
    '224.0.0.1',
    'https://192.168.1.1',
    '10.1.2',
    '10.-1.1.1',
  ])('rejects nonlocal or malformed IPv4 %s', (address) => {
    expect(validPeerAddress(address)).toBe(false);
  });
  it('requires the entire SHA-256 fingerprint, optionally colon separated', () => {
    expect(validFingerprint(peer.fingerprint)).toBe(true);
    expect(validFingerprint('ab'.repeat(32))).toBe(true);
    for (const value of [
      '123456',
      'AB:CD:EF',
      'ab'.repeat(31),
      'zz'.repeat(32),
      '<b>' + 'ab'.repeat(32) + '</b>',
    ])
      expect(validFingerprint(value)).toBe(false);
  });
});
