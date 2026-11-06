import { invoke, isTauri } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import type {
  AppSettings,
  AppSnapshot,
  Diagnostic,
  NativeFile,
  PeerInfo,
  TransferSnapshot,
} from './types';

export const isDesktop = () => isTauri();
export const getSnapshot = () => invoke<AppSnapshot>('get_snapshot');
export const saveSettings = (settings: AppSettings) =>
  invoke<AppSnapshot>('save_settings', { settings });
export const inspectFiles = (paths: string[]) => invoke<NativeFile[]>('inspect_files', { paths });
export const runDiagnostics = () => invoke<Diagnostic[]>('run_diagnostics');

export const getTransferSnapshot = () => invoke<TransferSnapshot>('get_transfer_snapshot');
export const connectPeer = (address: string, port: number, fingerprint: string) =>
  invoke<PeerInfo>('connect_peer', { address, port, fingerprint });
export const respondPair = (requestId: string, accept: boolean) =>
  invoke<void>('respond_pair', { requestId, accept });
export const forgetPeer = (fingerprint: string) => invoke<void>('forget_peer', { fingerprint });
export const sendFile = (fingerprint: string, path: string) =>
  invoke<string>('send_file', { fingerprint, path });
export const respondTransfer = (taskId: string, accept: boolean, directory: string | null) =>
  invoke<void>('respond_transfer', { taskId, accept, directory });
export const cancelTransfer = (taskId: string) => invoke<void>('cancel_transfer', { taskId });
export const refreshTransferResult = (taskId: string) =>
  invoke<void>('refresh_transfer_result', { taskId });

export async function chooseFiles() {
  const result = await open({ multiple: true, directory: false, title: '选择要发送的文件' });
  if (!result) return [];
  return inspectFiles(Array.isArray(result) ? result : [result]);
}

export async function chooseDirectory(defaultPath?: string) {
  return open({ directory: true, multiple: false, title: '选择接收目录', defaultPath });
}
