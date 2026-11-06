import { useEffect, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { ArrowLeftRight, FolderOpen, LoaderCircle, Plus, ShieldCheck } from 'lucide-react';
import * as bridge from './bridge';
import { errorMessage, formatBytes } from './utils';
import type { PairRequestInfo, PeerInfo, TransferInfo, TransferSnapshot } from './types';

const emptyTransfers: TransferSnapshot = { peers: [], pairRequests: [], tasks: [] };
export const verificationLabel = '已通过对方屏幕或可信渠道核对完整指纹';

export function useTransferSnapshot(initialized: boolean) {
  const [snapshot, setSnapshot] = useState<TransferSnapshot>(emptyTransfers);
  const [loaded, setLoaded] = useState(false);
  const [pollError, setPollError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);

  useEffect(() => {
    if (!initialized) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function poll() {
      try {
        const value = await bridge.getTransferSnapshot();
        if (disposed) return;
        setSnapshot(value);
        setLoaded(true);
        setPollError(null);
        setDismissed(null);
      } catch (reason) {
        if (!disposed) setPollError(`无法更新传输状态：${errorMessage(reason)}`);
      } finally {
        // Schedule only after the previous invocation settles; never overlap requests.
        if (!disposed) timer = setTimeout(() => void poll(), 750);
      }
    }
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [initialized]);

  return {
    snapshot,
    loaded,
    healthy: loaded && !pollError,
    pollError: pollError === dismissed ? null : pollError,
    stale: !!pollError,
    dismissPollError: () => setDismissed(pollError),
  };
}

export function isActiveTask(task: TransferInfo) {
  return ['hashing', 'awaitingAcceptance', 'ready', 'transferring', 'verifying'].includes(
    task.status,
  );
}

export function validPeerAddress(address: string) {
  const parts = address.trim().split('.');
  if (
    parts.length !== 4 ||
    parts.some((part) => !/^(0|[1-9]\d{0,2})$/.test(part) || Number(part) > 255)
  )
    return false;
  const [a, b] = parts.map(Number);
  return (
    a === 10 ||
    a === 127 ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a === 169 && b === 254)
  );
}

export function validFingerprint(value: string) {
  return /^(?:[a-f\d]{64}|(?:[a-f\d]{2}:){31}[a-f\d]{2})$/i.test(value.trim());
}

type ActionProps = { enabled: boolean; onError: (message: string | null) => void };

export function ManualConnection({
  enabled,
  onError,
  onConnected,
}: ActionProps & {
  onConnected: (peer: PeerInfo) => void;
}) {
  const [address, setAddress] = useState('');
  const [port, setPort] = useState('53318');
  const [fingerprint, setFingerprint] = useState('');
  const [verified, setVerified] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const busy = useRef(false);
  const validAddress = validPeerAddress(address);
  const validPort = /^\d+$/.test(port) && Number(port) >= 1024 && Number(port) <= 65535;
  const validPrint = validFingerprint(fingerprint);
  const allowed = enabled && validAddress && validPort && validPrint && verified && !connecting;

  async function connect(event: FormEvent) {
    event.preventDefault();
    if (!allowed || busy.current) return;
    busy.current = true;
    setConnecting(true);
    onError(null);
    try {
      const peer = await bridge.connectPeer(address.trim(), Number(port), fingerprint.trim());
      setVerified(false);
      onConnected(peer);
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      busy.current = false;
      setConnecting(false);
    }
  }

  return (
    <section className="card connection-card" aria-labelledby="manual-title">
      <div className="section-title">
        <h2 id="manual-title">手动连接设备</h2>
        <ShieldCheck size={18} />
      </div>
      <p className="section-description">
        从对方的「设备与安全」复制完整 SHA-256 指纹，并通过可信渠道核对。不要使用短码。
      </p>
      <form onSubmit={connect} noValidate>
        <fieldset disabled={!enabled || connecting}>
          <div className="connection-fields">
            <div className="field">
              <label htmlFor="peer-address">对方 IPv4 地址</label>
              <input
                id="peer-address"
                value={address}
                placeholder="192.168.1.10"
                autoComplete="off"
                aria-describedby="address-help"
                aria-invalid={!!address && !validAddress}
                onChange={(event) => {
                  setAddress(event.target.value);
                  setVerified(false);
                }}
              />
              <p id="address-help">仅限私有、回环或链路本地 IPv4 地址，不接受公网地址或主机名。</p>
            </div>
            <div className="field">
              <label htmlFor="peer-port">对方端口</label>
              <input
                id="peer-port"
                inputMode="numeric"
                value={port}
                aria-describedby="port-help"
                aria-invalid={!validPort}
                onChange={(event) => {
                  setPort(event.target.value);
                  setVerified(false);
                }}
              />
              <p id="port-help">整数 1024–65535</p>
            </div>
          </div>
          <div className="field fingerprint-field">
            <label htmlFor="peer-fingerprint">对方完整 SHA-256 指纹</label>
            <input
              id="peer-fingerprint"
              value={fingerprint}
              autoComplete="off"
              spellCheck={false}
              aria-describedby="fingerprint-help"
              aria-invalid={!!fingerprint && !validPrint}
              onChange={(event) => {
                setFingerprint(event.target.value);
                setVerified(false);
              }}
            />
            <p id="fingerprint-help">
              64 位十六进制字符，可使用冒号分隔每个字节。必须核对全部指纹。
            </p>
          </div>
          <label className="confirmation-check">
            <input
              type="checkbox"
              checked={verified}
              onChange={(event) => setVerified(event.target.checked)}
            />
            <span>{verificationLabel}</span>
          </label>
        </fieldset>
        {address && !validAddress && (
          <p className="validation-text">请输入有效的局域网 IPv4 地址。</p>
        )}
        {!validPort && <p className="validation-text">端口需要是 1024–65535 之间的整数。</p>}
        {fingerprint && !validPrint && (
          <p className="validation-text">请输入完整的 SHA-256 指纹，短码不能用于配对。</p>
        )}
        {connecting && (
          <p className="section-description" role="status">
            正在等待对方响应并批准配对，最长约 120 秒。请让对方核对你的完整指纹。
          </p>
        )}
        <button className="button primary" type="submit" disabled={!allowed}>
          {connecting ? <LoaderCircle size={16} className="spinning" /> : <Plus size={16} />}
          {connecting ? '等待对方配对响应…' : '请求配对'}
        </button>
      </form>
    </section>
  );
}

function PeerDetails({ peer }: { peer: PeerInfo }) {
  return (
    <div className="peer-details">
      <strong className="external-text">{peer.name}</strong>
      <span className="external-text">
        {peer.address}:{peer.port}
      </span>
      <code className="external-text">{peer.fingerprint}</code>
    </div>
  );
}

function TrustedPeer({
  peer,
  selected,
  onSelect,
  onForgotten,
  enabled,
  onError,
}: ActionProps & {
  peer: PeerInfo;
  selected: string;
  onSelect: (value: string) => void;
  onForgotten: (value: string) => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [revoked, setRevoked] = useState(false);
  const lock = useRef(false);
  async function forget() {
    if (!enabled || lock.current || revoked) return;
    lock.current = true;
    setBusy(true);
    onError(null);
    try {
      await bridge.forgetPeer(peer.fingerprint);
      setRevoked(true);
      setConfirming(false);
      onForgotten(peer.fingerprint);
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  return (
    <li>
      <label className="recipient-option">
        <input
          type="radio"
          name="recipient"
          aria-label={`选择接收设备 ${peer.name}`}
          value={peer.fingerprint}
          checked={selected === peer.fingerprint}
          disabled={!enabled || busy || revoked}
          onChange={() => onSelect(peer.fingerprint)}
        />
        <PeerDetails peer={peer} />
      </label>
      {revoked ? (
        <p className="section-description">已撤销信任，等待列表更新。</p>
      ) : (
        <>
          <button
            className="text-button danger-text"
            disabled={!enabled || busy}
            onClick={() => setConfirming(true)}
            aria-label={`撤销信任 ${peer.name}`}
          >
            撤销信任
          </button>
          {confirming && (
            <div className="revoke-confirmation">
              <p>撤销后需重新核对指纹并配对，才能再次发送文件。</p>
              <div className="action-row">
                <button className="button secondary" disabled={!enabled || busy} onClick={forget}>
                  确认撤销信任
                </button>
                <button
                  className="text-button"
                  disabled={busy}
                  onClick={() => setConfirming(false)}
                >
                  保留信任
                </button>
              </div>
            </div>
          )}
        </>
      )}
    </li>
  );
}

export function TrustedPeers({
  peers,
  selected,
  onSelect,
  onForgotten,
  enabled,
  onError,
}: ActionProps & {
  peers: PeerInfo[];
  selected: string;
  onSelect: (value: string) => void;
  onForgotten: (value: string) => void;
}) {
  return (
    <section className="card devices-card" aria-labelledby="trusted-title">
      <div className="section-title">
        <h2 id="trusted-title">可信设备 · 选择接收方</h2>
      </div>
      <p className="section-description">已配对不代表当前在线；连接状态以实际请求结果为准。</p>
      {peers.length === 0 ? (
        <p className="diagnostic-empty">暂无可信设备，请先手动配对。这里没有模拟设备。</p>
      ) : (
        <ul className="trusted-list">
          {peers.map((peer) => (
            <TrustedPeer
              key={peer.fingerprint}
              peer={peer}
              selected={selected}
              onSelect={onSelect}
              onForgotten={onForgotten}
              enabled={enabled}
              onError={onError}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

export function PairPrompt({
  request,
  enabled,
  onError,
}: ActionProps & { request: PairRequestInfo }) {
  const [verified, setVerified] = useState(false);
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const lock = useRef(false);
  const expired = Date.parse(request.expiresAt) <= Date.now();
  async function respond(accept: boolean) {
    if (!enabled || lock.current || submitted || (accept && (!verified || expired))) return;
    lock.current = true;
    setBusy(true);
    onError(null);
    try {
      await bridge.respondPair(request.id, accept);
      setSubmitted(true);
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  return (
    <section className="card approval-card" aria-label={`配对请求 ${request.name}`}>
      <h2>待确认的配对请求</h2>
      <PeerDetails peer={request} />
      <p className="section-description">
        调用方完整指纹如上。请与对方屏幕核对，不要仅凭设备名称信任。
      </p>
      <p className="section-description">
        {expired ? '请求已过期' : `有效期至 ${new Date(request.expiresAt).toLocaleString('zh-CN')}`}
      </p>
      <label className="confirmation-check">
        <input
          type="checkbox"
          checked={verified}
          disabled={!enabled || busy || submitted || expired}
          onChange={(event) => setVerified(event.target.checked)}
        />
        <span>{verificationLabel}</span>
      </label>
      <div className="action-row">
        <button
          className="button primary"
          disabled={!enabled || busy || submitted || !verified || expired}
          onClick={() => void respond(true)}
        >
          接受配对
        </button>
        <button
          className="button secondary"
          disabled={!enabled || busy || submitted}
          onClick={() => void respond(false)}
        >
          拒绝配对
        </button>
        {busy && <span role="status">正在提交…</span>}
        {submitted && <span role="status">配对响应已提交，等待状态更新。</span>}
      </div>
    </section>
  );
}

export function IncomingPrompt({
  task,
  defaultDirectory,
  enabled,
  onError,
}: ActionProps & {
  task: TransferInfo;
  defaultDirectory: string;
}) {
  const [directory, setDirectory] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const lock = useRef(false);
  const destination = directory ?? defaultDirectory;
  async function choose() {
    if (!enabled || lock.current || submitted) return;
    lock.current = true;
    setBusy(true);
    onError(null);
    try {
      const path = await bridge.chooseDirectory(destination || undefined);
      if (typeof path === 'string') setDirectory(path);
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  async function respond(accept: boolean) {
    if (!enabled || lock.current || submitted || (accept && !destination)) return;
    lock.current = true;
    setBusy(true);
    onError(null);
    try {
      // Send the displayed path, not an unsaved preferences draft or a hidden default.
      await bridge.respondTransfer(task.id, accept, accept ? destination : null);
      setSubmitted(true);
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  return (
    <section className="card approval-card" aria-label={`接收请求 ${task.fileName}`}>
      <h2>有人想发来一个文件</h2>
      <div className="peer-details">
        <strong className="external-text">{task.peerName}</strong>
        <code className="external-text">{task.peerFingerprint}</code>
      </div>
      <p className="incoming-file">
        <strong className="external-text">{task.fileName}</strong> · {formatBytes(task.size)}
      </p>
      <div className="field">
        <label htmlFor={`directory-${task.id}`}>
          本次接收目录{directory === null ? '（默认目录）' : ''}
        </label>
        <div className="input-with-button">
          <input
            id={`directory-${task.id}`}
            readOnly
            value={destination}
            placeholder="请先选择接收目录"
          />
          <button
            className="button secondary"
            disabled={!enabled || busy || submitted}
            onClick={choose}
          >
            <FolderOpen size={16} />
            选择本次接收目录
          </button>
        </div>
        <p>
          只有你明确接受后才接收文件，不覆盖已有文件。Windows 仅支持本地
          NTFS；不支持网络目录、链接或重解析点。
        </p>
      </div>
      <div className="action-row">
        <button
          className="button primary"
          disabled={!enabled || busy || submitted || !destination}
          onClick={() => void respond(true)}
        >
          接受文件
        </button>
        <button
          className="button secondary"
          disabled={!enabled || busy || submitted}
          onClick={() => void respond(false)}
        >
          拒绝文件
        </button>
        {busy && <span role="status">正在处理…</span>}
        {submitted && <span role="status">接收响应已提交，等待状态更新。</span>}
      </div>
    </section>
  );
}

const taskLabels: Record<TransferInfo['status'], string> = {
  hashing: '正在计算文件哈希',
  awaitingAcceptance: '等待接收方确认',
  ready: '已就绪，等待传输',
  transferring: '正在传输',
  verifying: '正在校验',
  completed: '已完成',
  rejected: '已拒绝',
  cancelled: '已取消',
  failed: '传输失败',
  unknown: '接收结果未知',
};

function TransferTask({ task, enabled, onError }: ActionProps & { task: TransferInfo }) {
  const [busy, setBusy] = useState(false);
  const [queried, setQueried] = useState(false);
  const [cancelSubmitted, setCancelSubmitted] = useState(false);
  const lock = useRef(false);
  const percent =
    task.size > 0
      ? Math.min(100, Math.max(0, (task.bytesTransferred / task.size) * 100))
      : task.status === 'completed'
        ? 100
        : 0;
  async function action(query: boolean) {
    if (!enabled || lock.current) return;
    lock.current = true;
    setBusy(true);
    setQueried(false);
    onError(null);
    try {
      if (query) {
        await bridge.refreshTransferResult(task.id);
        setQueried(true);
      } else {
        await bridge.cancelTransfer(task.id);
        setCancelSubmitted(true);
      }
    } catch (reason) {
      onError(errorMessage(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  return (
    <li className={`transfer-task task-${task.status}`} aria-label={`任务 ${task.id}`}>
      <div className="section-title">
        <h3 className="external-text">{task.fileName}</h3>
        <span className="task-status">{taskLabels[task.status]}</span>
      </div>
      <p className="section-description external-text">
        {task.direction === 'send' ? '发送给' : '接收自'} {task.peerName}
      </p>
      <code className="task-fingerprint external-text">{task.peerFingerprint}</code>
      <p className="section-description external-text">任务 ID：{task.id}</p>
      <progress max={100} value={percent} aria-label={`${task.fileName} 传输进度`} />
      <div className="transfer-metrics">
        <span>
          {formatBytes(task.bytesTransferred)} / {formatBytes(task.size)} · {percent.toFixed(1)}%
        </span>
        <span>{formatBytes(task.bytesPerSecond)} / 秒</span>
      </div>
      {task.status !== 'completed' && task.size > 0 && task.bytesTransferred >= task.size && (
        <p className="section-description">字节传输达到 100% 不代表成功，请以最终确认结果为准。</p>
      )}
      {task.error && <p className="validation-text external-text">{errorMessage(task.error)}</p>}
      {task.status === 'unknown' && (
        <p className="section-description">
          尚未确认对方是否保存成功。请查询接收结果，不要重复发送。
        </p>
      )}
      {task.status === 'completed' && task.direction === 'receive' && task.savedPath && (
        <p className="saved-path external-text">已保存到：{task.savedPath}</p>
      )}
      <div className="action-row">
        {isActiveTask(task) && (
          <button
            className="button secondary"
            disabled={!enabled || busy || cancelSubmitted}
            onClick={() => void action(false)}
          >
            {cancelSubmitted ? '取消请求已提交' : '取消传输'}
          </button>
        )}
        {task.status === 'unknown' && (
          <button
            className="button secondary"
            disabled={!enabled || busy}
            onClick={() => void action(true)}
          >
            查询接收结果
          </button>
        )}
        {busy && <span role="status">正在处理…</span>}
        {queried && task.status === 'unknown' && (
          <span role="status">查询已提交；若结果仍未知，可稍后再次查询。</span>
        )}
      </div>
    </li>
  );
}

export function TransferTasks({
  tasks,
  enabled,
  onError,
  onPrepare,
  submittedTaskId,
}: ActionProps & {
  tasks: TransferInfo[];
  onPrepare: () => void;
  submittedTaskId: string | null;
}) {
  return (
    <section className="card tasks-card" aria-labelledby="tasks-title">
      <div className="section-title">
        <h2 id="tasks-title">传输记录</h2>
        <span className="muted-tag">仅本次会话</span>
      </div>
      <p className="section-description">
        这里仅展示后端返回的真实任务。历史持久化、暂停与续传将在 P4 实现。
      </p>
      {submittedTaskId && !tasks.some((task) => task.id === submittedTaskId) && (
        <p className="inline-info external-text" role="status">
          已创建任务 {submittedTaskId}，等待后端状态更新。
        </p>
      )}
      {tasks.length === 0 ? (
        <div className="large-empty">
          <span className="empty-icon">
            <ArrowLeftRight size={34} strokeWidth={1.4} />
          </span>
          <h2>还没有传输任务</h2>
          <p>
            选择文件不会开始发送，也不会产生虚假的成功记录。
            <br />
            浏览器预览不能传输文件。
          </p>
          <button className="button secondary" onClick={onPrepare}>
            <Plus size={16} />
            先准备文件
          </button>
        </div>
      ) : (
        <ul className="transfer-list">
          {tasks.map((task) => (
            <TransferTask key={task.id} task={task} enabled={enabled} onError={onError} />
          ))}
        </ul>
      )}
    </section>
  );
}
