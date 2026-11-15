import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { ChangeEvent, DragEvent, FormEvent } from 'react';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import {
  ArrowDownToLine,
  ArrowLeftRight,
  ArrowRight,
  Check,
  ChevronRight,
  CircleAlert,
  Copy,
  File,
  Files,
  FolderOpen,
  Info,
  Laptop,
  LoaderCircle,
  Monitor,
  Network,
  Plus,
  RefreshCw,
  Settings2,
  ShieldCheck,
  Upload,
  X,
} from 'lucide-react';
import * as bridge from './bridge';
import { NearbyDevices, useDiscovery } from './DiscoveryUI';
import { errorMessage, formatBytes, mergeFiles, validateSettings } from './utils';
import {
  IncomingPrompt,
  isActiveTask,
  ManualConnection,
  PairPrompt,
  TransferTasks,
  TrustedPeers,
  useTransferSnapshot,
} from './TransferUI';
import type {
  AppSettings,
  AppSnapshot,
  Diagnostic,
  NativeFile,
  Page,
  PeerInfo,
  SelectedFile,
} from './types';

const navigation = [
  { id: 'send' as const, label: '发送文件', icon: Upload },
  { id: 'tasks' as const, label: '传输记录', icon: ArrowLeftRight },
  { id: 'security' as const, label: '设备与安全', icon: ShieldCheck },
  { id: 'settings' as const, label: '偏好设置', icon: Settings2 },
];
const headings: Record<Page, { title: string; detail: string }> = {
  send: { title: '把文件送到身边。', detail: '不绕路，不上云。让身边的设备，更近一点。' },
  tasks: {
    title: '每一次传输，都有迹可循。',
    detail: '发送和接收的进度与结果，将在这里集中呈现。',
  },
  security: { title: '先确认彼此，再开始连接。', detail: '同一个网络，不等于可以自动信任。' },
  settings: { title: '用你习惯的方式。', detail: '为这台设备设置一个好认的名字和接收位置。' },
};
const emptySettings: AppSettings = { deviceName: '', receiveDir: '', port: 53318 };

export default function App() {
  const desktop = useMemo(() => bridge.isDesktop(), []);
  const [page, setPage] = useState<Page>('send');
  const [snapshot, setSnapshot] = useState<AppSnapshot | null>(null);
  const [draft, setDraft] = useState<AppSettings>(emptySettings);
  const [loading, setLoading] = useState(desktop);
  const [saving, setSaving] = useState(false);
  const [adding, setAdding] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [files, setFiles] = useState<SelectedFile[]>([]);
  const selectedRef = useRef<SelectedFile[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const [diagnostics, setDiagnostics] = useState<Diagnostic[]>([]);
  const [diagnosing, setDiagnosing] = useState(false);
  const transfers = useTransferSnapshot(desktop && snapshot !== null);
  const discovery = useDiscovery(desktop && snapshot !== null);
  const [discoveredPeer, setDiscoveredPeer] = useState<PeerInfo | null>(null);
  const [pairing, setPairing] = useState(false);
  const [recipient, setRecipient] = useState('');
  const [sending, setSending] = useState(false);
  const sendLock = useRef(false);
  const [submittedTaskId, setSubmittedTaskId] = useState<string | null>(null);
  const serviceRunning = snapshot?.service.state === 'running';
  const transferEnabled = desktop && serviceRunning && transfers.healthy;
  const trustedRecipient = transfers.snapshot.peers.find((peer) => peer.fingerprint === recipient);
  const outboundActive = transfers.snapshot.tasks.some(
    (task) => task.direction === 'send' && (isActiveTask(task) || task.status === 'unknown'),
  );
  const awaitingTask =
    !!submittedTaskId && !transfers.snapshot.tasks.some((task) => task.id === submittedTaskId);
  const canSend =
    transferEnabled &&
    !!trustedRecipient &&
    files.length === 1 &&
    !!files[0].path &&
    !sending &&
    !adding &&
    !outboundActive &&
    !awaitingTask;

  useEffect(() => {
    if (submittedTaskId && transfers.snapshot.tasks.some((task) => task.id === submittedTaskId))
      setSubmittedTaskId(null);
  }, [submittedTaskId, transfers.snapshot.tasks]);

  const updateSelection = useCallback((next: SelectedFile[]) => {
    selectedRef.current = next;
    setFiles(next);
  }, []);
  const addSelection = useCallback(
    (incoming: SelectedFile[]) => {
      updateSelection(mergeFiles(selectedRef.current, incoming));
    },
    [updateSelection],
  );
  const addNativeFiles = useCallback(
    (incoming: NativeFile[]) => {
      addSelection(incoming.map((file) => ({ ...file, id: file.path })));
    },
    [addSelection],
  );

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    bridge
      .getSnapshot()
      .then((value) => {
        if (active) {
          setSnapshot(value);
          setDraft(value.settings);
        }
      })
      .catch((reason: unknown) => {
        if (active) setError(errorMessage(reason));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (disposed) return;
        if (event.payload.type === 'over') setDragging(true);
        if (event.payload.type === 'leave') setDragging(false);
        if (event.payload.type === 'drop') {
          setDragging(false);
          setAdding(true);
          setError(null);
          bridge
            .inspectFiles(event.payload.paths)
            .then((incoming) => {
              if (!disposed) {
                addNativeFiles(incoming);
                setPage('send');
              }
            })
            .catch((reason: unknown) => {
              if (!disposed) setError(errorMessage(reason));
            })
            .finally(() => {
              if (!disposed) setAdding(false);
            });
        }
      })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch((reason: unknown) => {
        if (!disposed) setError(`无法启用文件拖拽：${errorMessage(reason)}`);
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [desktop, addNativeFiles]);

  async function selectFiles() {
    setError(null);
    setNotice(null);
    if (!desktop) {
      inputRef.current?.click();
      return;
    }
    setAdding(true);
    try {
      addNativeFiles(await bridge.chooseFiles());
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setAdding(false);
    }
  }

  function addBrowserFiles(incoming: FileList | File[]) {
    setError(null);
    try {
      addSelection(
        Array.from(incoming).map((file) => ({
          id: `browser:${file.name}:${file.size}:${file.lastModified}`,
          name: file.name,
          size: file.size,
          modifiedAt: new Date(file.lastModified).toISOString(),
        })),
      );
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  function browserSelected(event: ChangeEvent<HTMLInputElement>) {
    if (event.target.files) addBrowserFiles(event.target.files);
    event.target.value = '';
  }

  function drop(event: DragEvent) {
    event.preventDefault();
    setDragging(false);
    if (desktop) return;
    const hasDirectory = Array.from(event.dataTransfer.items).some(
      (item) => item.webkitGetAsEntry?.()?.isDirectory,
    );
    if (hasDirectory) {
      setError('当前阶段仅支持普通文件；文件夹传输将在 P3 实现。');
      return;
    }
    addBrowserFiles(event.dataTransfer.files);
  }

  async function selectDirectory() {
    if (!desktop || !snapshot) return;
    setError(null);
    try {
      const path = await bridge.chooseDirectory(draft.receiveDir || undefined);
      if (typeof path === 'string') setDraft((current) => ({ ...current, receiveDir: path }));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    setNotice(null);
    const validation = validateSettings(draft);
    if (validation) {
      setError(validation);
      return;
    }
    if (!desktop) return;
    setError(null);
    setSaving(true);
    try {
      const value = await bridge.saveSettings({ ...draft, deviceName: draft.deviceName.trim() });
      setSnapshot(value);
      setDraft(value.settings);
      setNotice(
        value.service.port !== value.settings.port
          ? '设置已保存；新端口将在重启应用后生效。'
          : '设置已保存。',
      );
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setSaving(false);
    }
  }

  async function diagnose() {
    if (!desktop) return;
    setDiagnosing(true);
    setError(null);
    try {
      setDiagnostics(await bridge.runDiagnostics());
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setDiagnosing(false);
    }
  }

  async function copyFingerprint() {
    if (!snapshot) return;
    try {
      await navigator.clipboard.writeText(snapshot.identity.fingerprint);
      setNotice('完整证书指纹已复制。');
    } catch {
      setError('剪贴板不可用，请手动选择并复制指纹。');
    }
  }

  async function send() {
    if (!canSend || sendLock.current || !files[0]?.path) return;
    sendLock.current = true;
    setSending(true);
    setError(null);
    setNotice(null);
    try {
      const taskId = await bridge.sendFile(recipient, files[0].path);
      setSubmittedTaskId(taskId);
      setPage('tasks');
      setNotice(`已创建发送任务：${taskId}。进度与结果以后台状态为准。`);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      sendLock.current = false;
      setSending(false);
    }
  }

  const totalBytes = files.reduce((total, file) => total + file.size, 0);
  const currentHeading = headings[page];

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <a
          className="brand"
          href="#"
          onClick={(event) => {
            event.preventDefault();
            setPage('send');
          }}
          aria-label="邻传首页"
        >
          <span className="brand-mark">
            <ArrowLeftRight size={24} strokeWidth={2.3} />
          </span>
          <span>
            <strong>邻传</strong>
            <small>LANLINK</small>
          </span>
        </a>
        <div className="sidebar-label">你的本地连接</div>
        <nav aria-label="主要导航">
          {navigation.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              className={`nav-item ${page === id ? 'active' : ''}`}
              onClick={() => {
                setPage(id);
                setNotice(null);
              }}
              aria-label={label}
              aria-current={page === id ? 'page' : undefined}
            >
              <Icon size={19} />
              <span>{label}</span>
              {page === id && <span className="nav-dot" />}
            </button>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="local-first">
            <ShieldCheck size={19} />
            <div>
              <strong>为局域网而生</strong>
              <p>
                没有云端中转
                <br />
                文件留在你的网络里
              </p>
            </div>
          </div>
          <div className="version">
            <span className="version-dot" />
            P2 内部预览<span>v{snapshot?.version ?? '0.1.0'}</span>
          </div>
        </div>
      </aside>

      <div className="workspace">
        <header className="topbar">
          <div className="breadcrumb">
            工作空间
            <ChevronRight size={13} />
            <span>{navigation.find((item) => item.id === page)?.label}</span>
          </div>
          <span className={`connection-chip ${serviceRunning ? 'connected' : ''}`}>
            <span className="status-dot" />
            {desktop
              ? loading
                ? '正在读取本机信息'
                : serviceRunning
                  ? '本机 TLS 服务已启动'
                  : '本机服务待检查'
              : '浏览器预览 · 无原生连接'}
          </span>
        </header>

        <main>
          <div className="page-heading">
            <div>
              <div className="eyebrow">LOCAL CONNECTION, PERSONAL SPACE</div>
              <h1>{currentHeading.title}</h1>
              <p>{currentHeading.detail}</p>
            </div>
            <span className="phase-badge">P2 / 05</span>
          </div>
          <div className="stage-notice">
            <Info size={17} />
            <span>
              <strong>{desktop ? 'P2 内部预览' : '你正在查看浏览器预览'}</strong> ·
              桌面版支持自动发现邻传设备、核对完整指纹后配对、确认接收与单文件互传。Mac
              与双机验证尚未完成。
              {!desktop && ' 浏览器不能传输文件，也不会调用原生服务或模拟设备与结果。'}
            </span>
          </div>
          {error && (
            <div className="message error" role="alert">
              <CircleAlert size={18} />
              <span>{error}</span>
              <button
                className="icon-button"
                aria-label="关闭错误提示"
                onClick={() => setError(null)}
              >
                <X size={16} />
              </button>
            </div>
          )}
          {notice && (
            <div className="message success" role="status">
              <Check size={18} />
              <span>{notice}</span>
              <button
                className="icon-button"
                aria-label="关闭成功提示"
                onClick={() => setNotice(null)}
              >
                <X size={16} />
              </button>
            </div>
          )}
          {transfers.pollError && (
            <div className="message error" role="alert">
              <CircleAlert size={18} />
              <span>{transfers.pollError}</span>
              <button
                className="icon-button"
                aria-label="关闭状态更新错误"
                onClick={transfers.dismissPollError}
              >
                <X size={16} />
              </button>
            </div>
          )}
          {transfers.stale && (
            <p className="stale-notice">状态更新中断，当前显示上次快照；操作暂不可用，正在重试。</p>
          )}
          {desktop &&
            transfers.snapshot.pairRequests.map((request) => (
              <PairPrompt
                key={`${request.id}:${request.fingerprint}`}
                request={request}
                enabled={transferEnabled}
                onError={setError}
              />
            ))}
          {desktop &&
            transfers.snapshot.tasks
              .filter(
                (task) => task.direction === 'receive' && task.status === 'awaitingAcceptance',
              )
              .map((task) => (
                <IncomingPrompt
                  key={task.id}
                  task={task}
                  defaultDirectory={snapshot?.settings.receiveDir ?? ''}
                  enabled={transferEnabled}
                  onError={setError}
                />
              ))}
          {desktop && !snapshot && !loading && (
            <div className="message error">
              <Info size={18} />
              <span>本机初始化未完成。请处理错误后退出并重新打开应用。</span>
              <button className="button secondary" onClick={() => setPage('security')}>
                查看诊断
              </button>
            </div>
          )}

          {page === 'send' && (
            <div className="send-layout">
              <section className="card send-card" aria-labelledby="send-title">
                <div className="card-heading">
                  <div className="heading-icon">
                    <Upload size={19} />
                  </div>
                  <div>
                    <h2 id="send-title">准备发送</h2>
                    <p>选好文件，连接后再出发</p>
                  </div>
                  <span className="step-number">01</span>
                </div>
                <div
                  className={`dropzone ${dragging ? 'dragging' : ''}`}
                  onDragOver={(event) => {
                    event.preventDefault();
                    setDragging(true);
                  }}
                  onDragLeave={() => setDragging(false)}
                  onDrop={drop}
                >
                  <div className="file-illustration" aria-hidden="true">
                    <span className="file-back" />
                    <span className="file-front">
                      <Files size={35} strokeWidth={1.4} />
                    </span>
                    <span className="plus-bubble">
                      <Plus size={15} />
                    </span>
                  </div>
                  <h3>把文件拖到这里</h3>
                  <p>或从你的电脑中选择文件</p>
                  <button
                    className="button primary"
                    onClick={selectFiles}
                    disabled={adding || loading}
                  >
                    <Plus size={17} />
                    {adding ? '正在读取文件…' : '选择文件'}
                  </button>
                  <small>普通文件 · 可准备最多 128 个 · P2 每次只能发送一个文件</small>
                  <input
                    ref={inputRef}
                    type="file"
                    multiple
                    onChange={browserSelected}
                    className="visually-hidden"
                    aria-label="浏览器预览选择文件"
                    tabIndex={-1}
                  />
                </div>
                {files.length > 0 && (
                  <div className="selection">
                    <div className="selection-heading">
                      <span>
                        已选择 <strong>{files.length}</strong> 个文件 · {formatBytes(totalBytes)}
                      </span>
                      <button className="text-button" onClick={() => updateSelection([])}>
                        清空
                      </button>
                    </div>
                    <ul className="file-list">
                      {files.map((file) => (
                        <li key={file.id}>
                          <span className="file-icon">
                            <File size={20} />
                          </span>
                          <div>
                            <strong title={file.name}>{file.name}</strong>
                            <small>
                              {formatBytes(file.size)}
                              {!desktop ? ' · 仅浏览器本地选择' : ''}
                            </small>
                          </div>
                          <button
                            className="icon-button"
                            aria-label={`移除 ${file.name}`}
                            onClick={() =>
                              updateSelection(
                                selectedRef.current.filter((item) => item.id !== file.id),
                              )
                            }
                          >
                            <X size={16} />
                          </button>
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {files.length > 1 && (
                  <p className="validation-text">
                    P2 每次只能发送一个文件，请移除多余文件；不会自动发送第一个。
                  </p>
                )}
                {outboundActive && (
                  <p className="section-description">
                    已有未结束的发送任务，请到传输记录等待、取消或查询未知结果。
                  </p>
                )}
                <p className="section-description external-text">
                  {trustedRecipient
                    ? `接收方：${trustedRecipient.name}`
                    : '请在可信设备列表中选择接收方。'}
                </p>
                <div className="send-action">
                  <span>
                    <ShieldCheck size={15} />
                    接收方确认后才会发送
                  </span>
                  <button className="button primary" disabled={!canSend} onClick={send}>
                    {sending ? '正在创建任务…' : '发送所选文件'}
                    <ArrowRight size={15} />
                  </button>
                </div>
              </section>

              <div className="right-column">
                <TrustedPeers
                  peers={transfers.snapshot.peers}
                  selected={recipient}
                  onSelect={setRecipient}
                  onForgotten={(fingerprint) => {
                    if (recipient === fingerprint) setRecipient('');
                    setNotice('已撤销设备信任。');
                  }}
                  enabled={transferEnabled && !sending}
                  onError={setError}
                />
                <NearbyDevices
                  desktop={desktop}
                  serviceRunning={serviceRunning}
                  pairingEnabled={transferEnabled && !pairing}
                  discovery={discovery}
                  onConnect={setDiscoveredPeer}
                />
                <section className="card this-device" aria-labelledby="this-device-title">
                  <div className="section-title">
                    <h2 id="this-device-title">这台设备</h2>
                    <Monitor size={18} />
                  </div>
                  <strong className="device-name">
                    {snapshot?.device.name ??
                      (loading ? '正在读取…' : desktop ? '暂不可用' : '请在桌面应用中查看')}
                  </strong>
                  <span className="device-platform">
                    {snapshot
                      ? `${snapshot.device.platform} · ${snapshot.device.arch}`
                      : '设备信息不会在浏览器中模拟'}
                  </span>
                  <div className="device-address">
                    <Network size={14} />
                    <span>{snapshot?.device.addresses.join(' / ') || '暂无本机 IPv4 信息'}</span>
                  </div>
                  <button className="text-button" onClick={() => setPage('settings')}>
                    管理设备设置
                    <ChevronRight size={14} />
                  </button>
                </section>
              </div>
            </div>
          )}

          <div hidden={page !== 'send'} className="manual-connection-container">
            <ManualConnection
              enabled={transferEnabled}
              onError={setError}
              discoveredPeer={discoveredPeer}
              onConnecting={setPairing}
              onConnected={(peer) => {
                setRecipient(peer.fingerprint);
                setNotice('对方已批准配对，可信列表将随状态更新。');
              }}
            />
          </div>

          {page === 'tasks' && (
            <TransferTasks
              tasks={transfers.snapshot.tasks}
              enabled={transferEnabled}
              onError={setError}
              onPrepare={() => setPage('send')}
              submittedTaskId={submittedTaskId}
            />
          )}

          {page === 'security' && (
            <div className="security-layout">
              <section className="card identity-card">
                <div className="card-heading">
                  <div className="heading-icon">
                    <ShieldCheck size={20} />
                  </div>
                  <div>
                    <h2>本机设备身份</h2>
                    <p>完整 SHA-256 证书指纹</p>
                  </div>
                </div>
                <div className="fingerprint">
                  <code>{snapshot?.identity.fingerprint ?? '请在桌面应用中读取真实设备指纹'}</code>
                  <button
                    className="icon-button"
                    disabled={!snapshot}
                    onClick={copyFingerprint}
                    aria-label="复制完整证书指纹"
                  >
                    <Copy size={17} />
                  </button>
                </div>
                <dl className="details">
                  <div>
                    <dt>身份保护</dt>
                    <dd>{snapshot?.identity.storage ?? '使用系统凭据保护，浏览器不可用'}</dd>
                  </div>
                  <div>
                    <dt>设备标识</dt>
                    <dd>{snapshot?.device.id ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>创建时间</dt>
                    <dd>
                      {snapshot
                        ? new Date(snapshot.identity.createdAt).toLocaleString('zh-CN')
                        : '—'}
                    </dd>
                  </div>
                </dl>
                <div className="inline-info">
                  <Info size={16} />
                  <p>
                    首次配对双方都必须通过对方屏幕或可信渠道核对完整指纹，并明确批准。 单纯启用
                    HTTPS 不等于信任对方。可信设备的选择与撤销在「发送文件」页管理。
                  </p>
                </div>
              </section>
              <section className="card diagnostics-card">
                <div className="section-title">
                  <h2>本机诊断</h2>
                  <button
                    className="text-button"
                    onClick={diagnose}
                    disabled={!desktop || diagnosing}
                  >
                    {diagnosing ? (
                      <LoaderCircle size={15} className="spinning" />
                    ) : (
                      <RefreshCw size={15} />
                    )}
                    {diagnosing ? '检查中' : '运行检查'}
                  </button>
                </div>
                <p className="section-description">
                  验证本机 TLS 健康服务及基础环境，不扫描其他设备。
                </p>
                <div className="service-summary">
                  <span className={`status-dot ${serviceRunning ? 'green' : ''}`} />
                  <span>
                    {snapshot
                      ? `${snapshot.service.address}:${snapshot.service.port}`
                      : '原生服务未连接'}
                    <small>
                      {snapshot?.service.error ??
                        (snapshot?.service.scope === 'lan'
                          ? '监听 0.0.0.0 · 对方请使用本机局域网 IPv4 地址连接'
                          : snapshot
                            ? '仅回环地址 · 仅可本机连接'
                            : '原生服务未连接')}
                    </small>
                  </span>
                </div>
                {diagnostics.length > 0 ? (
                  <ul className="diagnostic-list">
                    {diagnostics.map((item, index) => (
                      <li key={`${item.name}:${index}`} className={item.status}>
                        {item.status === 'pass' ? (
                          <Check size={17} />
                        ) : item.status === 'fail' ? (
                          <CircleAlert size={17} />
                        ) : (
                          <Info size={17} />
                        )}
                        <div>
                          <strong>{item.name}</strong>
                          <p>{item.detail}</p>
                        </div>
                      </li>
                    ))}
                  </ul>
                ) : (
                  <div className="diagnostic-empty">
                    {desktop ? '点击运行检查，获取真实诊断结果。' : '诊断需要在桌面应用中运行。'}
                  </div>
                )}
              </section>
            </div>
          )}

          {page === 'settings' && (
            <form className="card settings-card" onSubmit={save} noValidate>
              <div className="card-heading">
                <div className="heading-icon">
                  <Laptop size={21} />
                </div>
                <div>
                  <h2>本机偏好</h2>
                  <p>设置只保存在这台电脑上</p>
                </div>
              </div>
              <fieldset disabled={!snapshot || saving}>
                <div className="field">
                  <label htmlFor="device-name">设备名称</label>
                  <p>让对方一眼认出这台电脑，最多 64 个字符。</p>
                  <input
                    id="device-name"
                    maxLength={128}
                    value={draft.deviceName}
                    placeholder={desktop ? '读取本机名称后可编辑' : '浏览器预览无法修改本机设置'}
                    onChange={(event) => setDraft({ ...draft, deviceName: event.target.value })}
                  />
                </div>
                <div className="field">
                  <label htmlFor="receive-dir">默认接收目录</label>
                  <p>接收前仍需确认；不会自动覆盖已有文件。</p>
                  <div className="input-with-button">
                    <input
                      id="receive-dir"
                      value={draft.receiveDir}
                      readOnly
                      placeholder="选择此电脑中的目录"
                    />
                    <button type="button" className="button secondary" onClick={selectDirectory}>
                      <FolderOpen size={16} />
                      选择目录
                    </button>
                  </div>
                </div>
                <div className="field port-field">
                  <label htmlFor="service-port">本机服务端口</label>
                  <p>
                    范围 1024–65535，修改后需重启应用；当前
                    {snapshot?.service.scope === 'lan' ? '监听局域网（0.0.0.0）' : '仅监听回环地址'}
                    。
                  </p>
                  <input
                    id="service-port"
                    type="number"
                    min={1024}
                    max={65535}
                    step={1}
                    value={Number.isNaN(draft.port) ? '' : draft.port}
                    onChange={(event) => setDraft({ ...draft, port: event.target.valueAsNumber })}
                  />
                </div>
              </fieldset>
              {snapshot && snapshot.service.port !== snapshot.settings.port && (
                <div className="inline-info">
                  <Info size={16} />
                  <p>
                    当前服务仍使用端口 {snapshot.service.port}，重启后将使用{' '}
                    {snapshot.settings.port}。
                  </p>
                </div>
              )}
              <div className="settings-actions">
                <span>
                  {desktop
                    ? '保存时由原生后端校验并原子写入'
                    : '请运行 npm run desktop:dev 使用原生设置'}
                </span>
                <button className="button primary" type="submit" disabled={!snapshot || saving}>
                  {saving ? <LoaderCircle size={16} className="spinning" /> : <Check size={16} />}
                  {saving ? '正在保存…' : '保存设置'}
                </button>
              </div>
            </form>
          )}

          <footer className="page-footer">
            <span>
              <Network size={13} />
              P2 内部预览 · Mac / 双机尚未验证
            </span>
            <span>
              <ArrowDownToLine size={13} />
              {desktop ? '单文件互传 · 双方明确确认' : '浏览器不能传输文件'}
            </span>
          </footer>
        </main>
      </div>
    </div>
  );
}
