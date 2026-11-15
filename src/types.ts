export interface AppSettings {
  deviceName: string;
  receiveDir: string;
  port: number;
}

export interface AppSnapshot {
  version: string;
  device: { id: string; name: string; platform: string; arch: string; addresses: string[] };
  settings: AppSettings;
  identity: { fingerprint: string; createdAt: string; storage: string };
  service: {
    state: 'running' | 'error';
    address: string;
    port: number;
    error: string | null;
    scope: 'lan' | 'loopback';
  };
}

export interface NativeFile {
  path: string;
  name: string;
  size: number;
  modifiedAt: string | null;
}

export interface SelectedFile {
  id: string;
  name: string;
  size: number;
  path?: string;
  modifiedAt: string | null;
}

export interface Diagnostic {
  name: string;
  status: 'pass' | 'fail' | 'info';
  detail: string;
}

export interface PeerInfo {
  fingerprint: string;
  name: string;
  address: string;
  port: number;
}

export interface PairRequestInfo extends PeerInfo {
  id: string;
  expiresAt: string;
}

export interface DiscoveredDevice {
  fingerprint: string;
  name: string;
  platform: string;
  addresses: string[];
  port: number;
}

export interface DiscoverySnapshot {
  state: 'running' | 'error';
  scanning: boolean;
  devices: DiscoveredDevice[];
  error: string | null;
}

export interface TransferInfo {
  id: string;
  direction: 'send' | 'receive';
  peerFingerprint: string;
  peerName: string;
  fileName: string;
  size: number;
  bytesTransferred: number;
  bytesPerSecond: number;
  status:
    | 'hashing'
    | 'awaitingAcceptance'
    | 'ready'
    | 'transferring'
    | 'verifying'
    | 'completed'
    | 'rejected'
    | 'cancelled'
    | 'failed'
    | 'unknown';
  error: string | null;
  savedPath: string | null;
}

export interface TransferSnapshot {
  peers: PeerInfo[];
  pairRequests: PairRequestInfo[];
  tasks: TransferInfo[];
}

export type Page = 'send' | 'tasks' | 'security' | 'settings';
