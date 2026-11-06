mod engine;
mod files;
mod identity;
#[cfg(test)]
mod p2_tests;
mod service;
mod settings;
mod storage;
mod tls;

use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{Manager, State, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceInfo {
    id: String,
    name: String,
    platform: String,
    arch: String,
    addresses: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdentityInfo {
    fingerprint: String,
    created_at: String,
    storage: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSnapshot {
    version: String,
    device: DeviceInfo,
    settings: settings::Settings,
    identity: IdentityInfo,
    service: service::ServiceSnapshot,
}

struct Backend {
    device: DeviceInfo,
    settings: settings::Settings,
    identity: Arc<identity::Identity>,
    engine: Arc<engine::Engine>,
    service: Option<service::ServiceHandle>,
}

impl Backend {
    fn snapshot(&self) -> Result<AppSnapshot, String> {
        let service = self
            .service
            .as_ref()
            .ok_or("应用正在退出，本机服务已关闭。")?;
        let mut device = self.device.clone();
        device.name = self.settings.device_name.clone();
        Ok(AppSnapshot {
            version: env!("CARGO_PKG_VERSION").into(),
            device,
            settings: self.settings.clone(),
            identity: IdentityInfo {
                fingerprint: self.identity.fingerprint.clone(),
                created_at: self.identity.created_at.clone(),
                storage: identity::storage_name().into(),
            },
            service: service.snapshot(),
        })
    }
}

struct NativeState {
    config_dir: Result<PathBuf, String>,
    backend: Arc<Mutex<Result<Backend, String>>>,
    closing: AtomicBool,
    exit_confirmed: AtomicBool,
}

async fn initialize(config_dir: &Result<PathBuf, String>) -> Result<Backend, String> {
    let config_dir = config_dir.as_ref().map_err(Clone::clone)?;
    let settings = settings::load_or_create(config_dir)?;
    let identity = Arc::new(identity::load_or_create(config_dir)?);
    let mut addresses: Vec<String> = if_addrs::get_if_addrs()
        .map_err(|error| format!("无法枚举本机网卡地址：{error}"))?
        .into_iter()
        .filter(|interface| interface.ip().is_ipv4() && !interface.ip().is_loopback())
        .map(|interface| interface.ip().to_string())
        .collect();
    addresses.sort();
    addresses.dedup();
    let device = DeviceInfo {
        id: identity.id(),
        name: settings.device_name.clone(),
        platform: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        addresses,
    };
    let port = u16::try_from(settings.port).map_err(|_| "设置中的监听端口超出范围。")?;
    let engine = engine::Engine::new(
        config_dir.clone(),
        identity.clone(),
        settings.device_name.clone(),
    )?;
    let service = service::start(&identity, port, engine.clone()).await;
    Ok(Backend {
        device,
        settings,
        identity,
        engine,
        service: Some(service),
    })
}

#[tauri::command]
fn get_snapshot(state: State<'_, NativeState>) -> Result<AppSnapshot, String> {
    let backend = state.backend.lock().map_err(|_| "本机应用状态锁异常。")?;
    backend.as_ref().map_err(Clone::clone)?.snapshot()
}

#[tauri::command(rename_all = "camelCase")]
async fn save_settings(
    settings: settings::Settings,
    state: State<'_, NativeState>,
) -> Result<AppSnapshot, String> {
    // Serialize saves so the in-memory state cannot diverge from the committed file.
    let config_dir = state.config_dir.as_ref().map_err(Clone::clone)?.clone();
    let backend = Arc::clone(&state.backend);
    tauri::async_runtime::spawn_blocking(move || {
        let mut backend = backend.lock().map_err(|_| "本机应用状态锁异常。")?;
        let backend = backend.as_mut().map_err(|error| error.clone())?;
        backend.settings = settings::save(&config_dir, settings)?;
        backend
            .engine
            .set_name(backend.settings.device_name.clone());
        // Port changes apply next launch; never pretend to rebind the current service.
        backend.snapshot()
    })
    .await
    .map_err(|error| format!("设置保存任务失败：{error}"))?
}

#[tauri::command(rename_all = "camelCase")]
async fn inspect_files(paths: Vec<String>) -> Result<Vec<files::FileInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || files::inspect(paths))
        .await
        .map_err(|error| format!("文件信息检查任务失败：{error}"))?
}

fn transfer_engine(
    state: &NativeState,
    require_running: bool,
) -> Result<Arc<engine::Engine>, String> {
    if state.closing.load(Ordering::Acquire) {
        return Err("应用正在处理退出请求。".into());
    }
    let backend = state.backend.lock().map_err(|_| "本机应用状态锁异常。")?;
    let backend = backend.as_ref().map_err(Clone::clone)?;
    if require_running
        && !backend
            .service
            .as_ref()
            .is_some_and(|service| service.snapshot().state == service::ServiceState::Running)
    {
        return Err("本机监听服务未启动，请检查端口占用并重启应用。".into());
    }
    Ok(backend.engine.clone())
}

#[tauri::command]
fn get_transfer_snapshot(
    state: State<'_, NativeState>,
) -> Result<engine::TransferSnapshot, String> {
    transfer_engine(&state, false)?.snapshot()
}

#[tauri::command(rename_all = "camelCase")]
async fn connect_peer(
    address: String,
    port: u16,
    fingerprint: String,
    state: State<'_, NativeState>,
) -> Result<engine::PeerInfo, String> {
    transfer_engine(&state, true)?
        .connect(address, port, fingerprint)
        .await
}

#[tauri::command(rename_all = "camelCase")]
async fn respond_pair(
    request_id: String,
    accept: bool,
    state: State<'_, NativeState>,
) -> Result<(), String> {
    let engine = transfer_engine(&state, false)?;
    tauri::async_runtime::spawn_blocking(move || engine.respond_pair(request_id, accept))
        .await
        .map_err(|error| format!("配对处理失败：{error}"))?
}

#[tauri::command(rename_all = "camelCase")]
async fn forget_peer(fingerprint: String, state: State<'_, NativeState>) -> Result<(), String> {
    let engine = transfer_engine(&state, false)?;
    tauri::async_runtime::spawn_blocking(move || engine.forget_peer(fingerprint))
        .await
        .map_err(|error| format!("撤销信任失败：{error}"))?
}

#[tauri::command(rename_all = "camelCase")]
async fn send_file(
    fingerprint: String,
    path: String,
    state: State<'_, NativeState>,
) -> Result<String, String> {
    transfer_engine(&state, true)?.send(fingerprint, path)
}

#[tauri::command(rename_all = "camelCase")]
async fn respond_transfer(
    task_id: String,
    accept: bool,
    directory: Option<String>,
    state: State<'_, NativeState>,
) -> Result<(), String> {
    let engine = transfer_engine(&state, false)?;
    tauri::async_runtime::spawn_blocking(move || {
        engine.respond_transfer(task_id, accept, directory)
    })
    .await
    .map_err(|error| format!("接收确认失败：{error}"))?
}

#[tauri::command(rename_all = "camelCase")]
async fn cancel_transfer(task_id: String, state: State<'_, NativeState>) -> Result<(), String> {
    transfer_engine(&state, false)?.cancel(task_id)
}

#[tauri::command(rename_all = "camelCase")]
async fn refresh_transfer_result(
    task_id: String,
    state: State<'_, NativeState>,
) -> Result<(), String> {
    transfer_engine(&state, false)?
        .refresh_result(task_id)
        .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic {
    name: String,
    status: DiagnosticStatus,
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum DiagnosticStatus {
    Pass,
    Fail,
    Info,
}

fn diagnostic(name: &str, status: DiagnosticStatus, detail: impl Into<String>) -> Diagnostic {
    Diagnostic {
        name: name.into(),
        status,
        detail: detail.into(),
    }
}

#[tauri::command]
async fn run_diagnostics(state: State<'_, NativeState>) -> Result<Vec<Diagnostic>, String> {
    let mut results = Vec::new();
    match &state.config_dir {
        Ok(directory) => {
            let path = settings::config_file(directory);
            match settings::read(&path) {
                Ok(_) => results.push(diagnostic(
                    "配置路径",
                    DiagnosticStatus::Pass,
                    format!(
                        "配置可读取且校验通过：{}。设置采用同目录临时文件原子替换。",
                        path.display()
                    ),
                )),
                Err(error) => results.push(diagnostic(
                    "配置路径",
                    DiagnosticStatus::Fail,
                    format!("{}：{error}", path.display()),
                )),
            }
        }
        Err(error) => results.push(diagnostic(
            "配置路径",
            DiagnosticStatus::Fail,
            error.clone(),
        )),
    }

    // Retain identity internally for mutual TLS without holding the state lock across awaits.
    let probe_context = {
        let backend = state.backend.lock().map_err(|_| "本机应用状态锁异常。")?;
        match backend.as_ref() {
            Ok(backend) => {
                match settings::validate(backend.settings.clone()) {
                    Ok(_) => results.push(diagnostic(
                        "接收目录",
                        DiagnosticStatus::Pass,
                        "已保存的接收目录存在；接收时仍需重新确认写入权限。",
                    )),
                    Err(error) => results.push(diagnostic(
                        "接收目录",
                        DiagnosticStatus::Fail,
                        format!("{error} 请在偏好设置中重新选择目录。"),
                    )),
                }
                results.push(diagnostic(
                    "系统身份存储",
                    DiagnosticStatus::Pass,
                    format!(
                        "本次启动已成功从 {} 读取或首次保存身份；证书与私钥匹配，私钥未明文写入磁盘。",
                        identity::storage_name()
                    ),
                ));
                backend.service.as_ref().map(|service| {
                    (
                        backend.identity.clone(),
                        service.snapshot(),
                        backend.settings.port,
                    )
                })
            }
            Err(error) => {
                results.push(diagnostic(
                    "系统身份存储",
                    DiagnosticStatus::Fail,
                    format!(
                        "原生基础初始化未完成，无法确认 {} 身份可用：{error}",
                        identity::storage_name()
                    ),
                ));
                None
            }
        }
    };
    match probe_context {
        Some((identity, snapshot, configured_port)) => {
            if snapshot.state == service::ServiceState::Running {
                match service::probe(&identity, snapshot.port).await {
                    Ok(detail) => results.push(diagnostic(
                        "本机 TLS 健康检查",
                        DiagnosticStatus::Pass,
                        detail,
                    )),
                    Err(error) => results.push(diagnostic(
                        "本机 TLS 健康检查",
                        DiagnosticStatus::Fail,
                        error,
                    )),
                }
            } else {
                results.push(diagnostic(
                    "本机 TLS 健康检查",
                    DiagnosticStatus::Fail,
                    snapshot
                        .error
                        .unwrap_or_else(|| "本机健康服务未运行。".into()),
                ));
            }
            if configured_port != u32::from(snapshot.port) {
                results.push(diagnostic(
                    "端口设置",
                    DiagnosticStatus::Info,
                    format!(
                        "当前服务端口为 {}，已保存端口 {configured_port} 将在重启应用后生效。",
                        snapshot.port
                    ),
                ));
            }
        }
        None => results.push(diagnostic(
            "本机 TLS 健康检查",
            DiagnosticStatus::Fail,
            "服务未启动，未发送网络请求。请先处理初始化错误。",
        )),
    }
    results.push(diagnostic(
        "局域网开放状态",
        DiagnosticStatus::Info,
        "P2 监听 IPv4，接受私有、链路本地和回环来源；仅手动连接，不扫描设备。文件接口要求固定身份和逐次接收批准。若不可达，请检查两端网络和应用防火墙许可；不要关闭防火墙。",
    ));
    Ok(results)
}

fn navigation_allowed(url: &tauri::Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let packaged = matches!(
        (url.scheme(), url.host_str(), url.port()),
        ("tauri", Some("localhost"), None) | ("http", Some("tauri.localhost"), None)
    );
    let development = cfg!(debug_assertions)
        && url.scheme() == "http"
        && url.host_str() == Some("localhost")
        && url.port() == Some(1420);
    packaged || development
}

fn finish_exit(app: tauri::AppHandle, engine: Option<Arc<engine::Engine>>) {
    tauri::async_runtime::spawn(async move {
        if let Some(engine) = engine {
            engine.shutdown().await;
        }
        let service = {
            let state = app.state::<NativeState>();
            let service = state.backend.lock().ok().and_then(|mut backend| {
                backend
                    .as_mut()
                    .ok()
                    .and_then(|backend| backend.service.take())
            });
            service
        };
        if let Some(service) = service {
            service.shutdown().await;
        }
        app.state::<NativeState>()
            .exit_confirmed
            .store(true, Ordering::Release);
        app.exit(0);
    });
}

fn request_exit(app: &tauri::AppHandle) {
    let state = app.state::<NativeState>();
    if state.closing.swap(true, Ordering::AcqRel) {
        return;
    }
    let engine = state
        .backend
        .lock()
        .ok()
        .and_then(|backend| backend.as_ref().ok().map(|backend| backend.engine.clone()));
    if engine.as_ref().is_some_and(|engine| engine.has_active()) {
        let handle = app.clone();
        app.dialog()
            .message("仍有配对或传输任务。退出将停止任务，未完成文件不会作为成功文件保存。")
            .title("退出邻传？")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "停止任务并退出".into(),
                "继续传输".into(),
            ))
            .show(move |confirmed| {
                if confirmed {
                    finish_exit(handle, engine);
                } else {
                    handle
                        .state::<NativeState>()
                        .closing
                        .store(false, Ordering::Release);
                }
            });
    } else {
        finish_exit(app.clone(), engine);
    }
}

pub fn run() {
    let application = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                request_exit(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            save_settings,
            inspect_files,
            run_diagnostics,
            get_transfer_snapshot,
            connect_peer,
            respond_pair,
            forget_peer,
            send_file,
            respond_transfer,
            cancel_transfer,
            refresh_transfer_result
        ])
        .setup(|app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|error| format!("无法定位应用配置目录：{error}"));
            // Initialization errors are stored and returned by IPC, not raised from setup.
            let backend = tauri::async_runtime::block_on(initialize(&config_dir));
            app.manage(NativeState {
                config_dir,
                backend: Arc::new(Mutex::new(backend)),
                closing: AtomicBool::new(false),
                exit_confirmed: AtomicBool::new(false),
            });
            let config = app.config().app.windows.first().ok_or("缺少主窗口配置。")?;
            WebviewWindowBuilder::from_config(app, config)?
                .on_navigation(navigation_allowed)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!());
    match application {
        Ok(application) => application.run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if !app
                    .state::<NativeState>()
                    .exit_confirmed
                    .load(Ordering::Acquire)
                {
                    api.prevent_exit();
                    request_exit(app);
                }
            }
        }),
        Err(error) => eprintln!("邻传桌面窗口初始化失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_rejects_remote_and_script_urls() {
        for url in [
            "https://example.com",
            "https://localhost:1420",
            "http://localhost:1421",
            "http://localhost.example.com:1420",
            "http://user:pass@localhost:1420",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://tauri.localhost",
        ] {
            assert!(
                !navigation_allowed(&tauri::Url::parse(url).unwrap()),
                "{url}"
            );
        }
        assert!(navigation_allowed(
            &tauri::Url::parse("tauri://localhost/index.html").unwrap()
        ));
        assert!(navigation_allowed(
            &tauri::Url::parse("http://tauri.localhost/index.html").unwrap()
        ));
        assert_eq!(
            navigation_allowed(&tauri::Url::parse("http://localhost:1420").unwrap()),
            cfg!(debug_assertions)
        );
    }

    #[tokio::test]
    async fn saving_a_port_does_not_change_the_running_service_port() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Arc::new(identity::generate().unwrap());
        let engine = engine::Engine::new(
            directory.path().to_owned(),
            identity.clone(),
            "测试设备".into(),
        )
        .unwrap();
        let service =
            service::start_on(&identity, std::net::Ipv4Addr::LOCALHOST, 0, engine.clone()).await;
        let actual_port = service.snapshot().port;
        let settings = settings::Settings {
            device_name: "测试设备".into(),
            receive_dir: directory.path().to_str().unwrap().into(),
            port: u32::from(actual_port),
        };
        let mut backend = Backend {
            device: DeviceInfo {
                id: identity.id(),
                name: settings.device_name.clone(),
                platform: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                addresses: vec!["127.0.0.1".into()],
            },
            settings,
            identity,
            engine,
            service: Some(service),
        };
        let mut changed = backend.settings.clone();
        changed.port = if actual_port == 53318 { 53319 } else { 53318 };
        changed.device_name = "新的设备名".into();
        backend.settings = settings::save(directory.path(), changed.clone()).unwrap();
        let snapshot = backend.snapshot().unwrap();
        assert_eq!(snapshot.service.port, actual_port);
        assert_eq!(snapshot.settings.port, changed.port);
        assert_eq!(snapshot.device.name, changed.device_name);
        assert_eq!(
            settings::read(&settings::config_file(directory.path())).unwrap(),
            changed
        );
        assert!(service::probe(&backend.identity, actual_port).await.is_ok());
        backend.engine.shutdown().await;
        backend.service.take().unwrap().shutdown().await;
    }

    #[test]
    fn ipc_uses_camel_case_and_has_no_private_identity_fields() {
        let value = serde_json::to_value(IdentityInfo {
            fingerprint: "public fingerprint".into(),
            created_at: "2026-10-30T00:00:00Z".into(),
            storage: "test storage".into(),
        })
        .unwrap();
        assert!(value.get("createdAt").is_some());
        assert_eq!(value.as_object().unwrap().len(), 3);
        let value = serde_json::to_value(settings::Settings {
            device_name: "设备".into(),
            receive_dir: "test".into(),
            port: 53318,
        })
        .unwrap();
        assert!(value.get("deviceName").is_some());
        assert!(value.get("receiveDir").is_some());
        assert!(value.get("device_name").is_none());
    }
}
