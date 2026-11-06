import type { AppSettings, SelectedFile } from './types';

export function formatBytes(bytes: number) {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${new Intl.NumberFormat('zh-CN', { maximumFractionDigits: index > 0 ? 1 : 0 }).format(bytes / 1024 ** index)} ${units[index]}`;
}

export function mergeFiles(current: SelectedFile[], incoming: SelectedFile[]) {
  const unique = new Map(current.map((file) => [file.id, file]));
  for (const file of incoming) unique.set(file.id, file);
  if (unique.size > 128) throw new Error('当前最多可选择 128 个文件，请减少文件数量。');
  return [...unique.values()];
}

export function validateSettings(settings: AppSettings) {
  const name = settings.deviceName.trim();
  if (!name || [...name].length > 64 || /\p{Cc}/u.test(name))
    return '设备名称需要 1–64 个字符，且不能包含控制字符。';
  if (!settings.receiveDir.trim()) return '请选择接收目录。';
  if (!Number.isInteger(settings.port) || settings.port < 1024 || settings.port > 65535)
    return '端口需要是 1024–65535 之间的整数。';
  return null;
}

const transferErrors: Record<string, string> = {
  peer_request_failed:
    '连接失败：请核对完整指纹、地址与端口，并确认对方已启动应用和允许应用通过防火墙。',
  peer_request_timeout: '对方响应超时，请检查两端网络后重试。',
  pair_rejected: '对方拒绝了配对请求。',
  pair_expired: '配对请求已过期，请重新核对指纹后发起。',
  untrusted_peer: '该设备尚未配对或信任已撤销，请重新核对指纹。',
  endpoint_identity_changed_revoke_first:
    '此地址的设备身份发生变化；请先撤销旧信任，再核对新指纹。',
  peer_endpoint_changed_revoke_first: '设备地址或端口已变化；请先撤销旧信任，再重新配对。',
  direction_busy: '当前方向已有未结束任务，请等待完成或查询未知结果。',
  history_full: '本次会话记录已满；请确认所有任务结果后重启应用。',
  source_open_failed: '无法安全打开源文件，请检查文件权限、占用和路径；不支持链接及重解析点。',
  source_changed: '源文件在读取期间发生变化，未确认传输成功。',
  destination_create_failed:
    '无法创建接收文件，请检查目录权限和文件系统；Windows 仅支持本地 NTFS。',
  destination_space_unavailable: '无法确认接收目录的可用空间。',
  destination_space_insufficient: '接收目录的可用空间不足。',
  destination_write_failed: '写入失败，请检查磁盘空间、权限或设备连接。',
  destination_commit_failed: '无法确认文件提交成功，请检查接收目录及同名文件，不要直接重发。',
  digest_mismatch: '文件摘要不一致，未保存为完成文件。',
  length_mismatch: '实际文件长度与请求不一致，未确认成功。',
  body_interrupted: '传输连接中断，未完成的临时文件将清理。',
  acceptance_expired: '接收确认已超时，请重新发起传输。',
  remote_transfer_failed: '对方接收失败，请在对方的传输记录查看具体原因。',
  remote_result_unknown_refresh_required: '最终确认未收到，请查询接收结果；不要重复发送。',
  trust_store_in_use: '另一邻传进程正在使用本机信任库，请关闭重复实例后重启。',
};

export function errorMessage(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return transferErrors[message] ?? message;
}
