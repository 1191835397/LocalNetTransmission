import { useEffect, useRef, useState } from 'react';
import { LoaderCircle, RefreshCw, Wifi } from 'lucide-react';
import * as bridge from './bridge';
import { errorMessage } from './utils';
import type { DiscoverySnapshot, PeerInfo } from './types';

export function useDiscovery(initialized: boolean) {
  const [snapshot, setSnapshot] = useState<DiscoverySnapshot | null>(null);
  const [pollError, setPollError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const active = useRef(false);
  const busy = useRef(false);
  const revision = useRef(0);

  useEffect(() => {
    if (!initialized) return;
    active.current = true;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function poll() {
      const current = revision.current;
      try {
        const value = await bridge.getDiscoverySnapshot();
        if (!disposed && current === revision.current) {
          setSnapshot(value);
          setPollError(null);
        }
      } catch (reason) {
        if (!disposed && current === revision.current)
          setPollError(`无法更新附近设备：${errorMessage(reason)}`);
      } finally {
        if (!disposed) timer = setTimeout(() => void poll(), 1000);
      }
    }
    void poll();
    return () => {
      disposed = true;
      active.current = false;
      clearTimeout(timer);
    };
  }, [initialized]);

  async function refresh() {
    if (!initialized || busy.current || (snapshot?.scanning && !pollError)) return;
    busy.current = true;
    revision.current++;
    setRefreshing(true);
    setRefreshError(null);
    try {
      const value = await bridge.discoverDevices();
      if (active.current) {
        revision.current++;
        setSnapshot(value);
        setPollError(null);
      }
    } catch (reason) {
      if (active.current) setRefreshError(`无法重新发现设备：${errorMessage(reason)}`);
    } finally {
      busy.current = false;
      if (active.current) setRefreshing(false);
    }
  }

  return {
    snapshot,
    error: refreshError ?? pollError ?? snapshot?.error,
    stale: !!pollError,
    refreshing: refreshing || (!!snapshot?.scanning && !pollError),
    refresh,
  };
}

export function NearbyDevices({
  desktop,
  serviceRunning,
  pairingEnabled,
  discovery,
  onConnect,
}: {
  desktop: boolean;
  serviceRunning: boolean;
  pairingEnabled: boolean;
  discovery: ReturnType<typeof useDiscovery>;
  onConnect: (peer: PeerInfo) => void;
}) {
  const running = discovery.snapshot?.state === 'running';
  const devices = discovery.snapshot?.devices ?? [];
  return (
    <section className="card devices-card discovery-card" aria-labelledby="devices-title">
      <div className="section-title">
        <h2 id="devices-title">附近设备</h2>
        <span className="muted-tag">
          {!desktop
            ? '仅桌面可用'
            : discovery.refreshing
              ? '正在发现'
              : discovery.stale
                ? '状态待更新'
                : running
                  ? '自动发现已开启'
                  : '发现未就绪'}
        </span>
      </div>
      <p className="section-description">
        仅发现同一局域网内运行邻传的设备，不扫描整个网段。发现信息未经身份验证，配对前仍需核对完整指纹。
      </p>
      <button
        className="button secondary discovery-button"
        disabled={!desktop || !serviceRunning || discovery.refreshing}
        onClick={() => void discovery.refresh()}
      >
        {discovery.refreshing ? (
          <LoaderCircle size={16} className="spinning" />
        ) : (
          <RefreshCw size={16} />
        )}
        {discovery.refreshing ? '正在发现设备…' : '发现本地设备'}
      </button>
      {discovery.error && (
        <p className="validation-text" role="alert">
          {discovery.error}
        </p>
      )}
      {discovery.stale && (
        <p className="section-description">当前显示上次发现结果，状态恢复前不能通过此列表连接。</p>
      )}
      {devices.length > 0 ? (
        <ul className="discovered-list">
          {devices.map((device) => (
            <li key={device.fingerprint}>
              <div className="peer-details">
                <strong className="external-text">{device.name}</strong>
                <span className="external-text">{device.platform} · 待核验设备</span>
              </div>
              <div className="discovered-addresses">
                {device.addresses.map((address) => (
                  <button
                    key={address}
                    className="text-button external-text"
                    disabled={!pairingEnabled || !running || discovery.stale}
                    aria-label={`连接 ${device.name} (${address}:${device.port})`}
                    onClick={() =>
                      onConnect({
                        name: device.name,
                        fingerprint: device.fingerprint,
                        address,
                        port: device.port,
                      })
                    }
                  >
                    {address}:{device.port} · 连接
                  </button>
                ))}
              </div>
            </li>
          ))}
        </ul>
      ) : (
        <div className="empty-devices">
          <div className="radar" aria-hidden="true">
            <Wifi size={25} />
          </div>
          <h3>
            {!desktop
              ? '浏览器无法发现局域网设备'
              : !serviceRunning
                ? '请先启动本机服务'
                : discovery.refreshing
                  ? '正在查找附近的邻传设备'
                  : '暂未发现附近设备'}
          </h3>
          <p>
            {desktop
              ? '请让对方打开邻传并连接同一局域网；设备离线后会从列表移除。'
              : '请使用桌面应用进行自动发现、配对和文件传输。'}
          </p>
        </div>
      )}
      {desktop && (
        <p className="section-description">
          找不到设备？检查专用网络的 mDNS（UDP
          5353）与应用防火墙许可，或使用下方手动连接；访客网络及 AP 隔离可能阻止通信。
        </p>
      )}
    </section>
  );
}
