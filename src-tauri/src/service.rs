use crate::{
    engine::Engine,
    identity::{self, Identity},
    tls,
};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    body::Incoming,
    header::{HeaderValue, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{oneshot, Semaphore},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

const MAX_CONNECTIONS: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEALTH_BYTES: usize = 1024;
const HEALTH_BODY: &str = concat!(
    "{\"version\":\"",
    env!("CARGO_PKG_VERSION"),
    "\",\"status\":\"ok\"}"
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceState {
    Running,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub state: ServiceState,
    pub address: String,
    pub port: u16,
    pub error: Option<String>,
    pub scope: ServiceScope,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceScope {
    Lan,
}

pub struct ServiceHandle {
    snapshot: Arc<RwLock<ServiceSnapshot>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ServiceHandle {
    pub fn snapshot(&self) -> ServiceSnapshot {
        let mut snapshot = match self.snapshot.read() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => {
                let mut snapshot = poisoned.into_inner().clone();
                snapshot.state = ServiceState::Error;
                snapshot.error = Some("本机服务状态锁异常；请重启应用。".into());
                snapshot
            }
        };
        if snapshot.state == ServiceState::Running
            && self.task.as_ref().is_some_and(JoinHandle::is_finished)
        {
            snapshot.state = ServiceState::Error;
            snapshot.error = Some("本机 TLS 服务已停止。".into());
        }
        snapshot
    }

    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut task) = self.task.take() {
            if timeout(Duration::from_secs(35), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn start(identity: &Identity, port: u16, engine: Arc<Engine>) -> ServiceHandle {
    start_on(identity, Ipv4Addr::UNSPECIFIED, port, engine).await
}

fn bind_listener(address: Ipv4Addr, port: u16) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            setsockopt, WSAGetLastError, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
        };
        let exclusive = 1i32;
        if unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                (&exclusive as *const i32).cast(),
                std::mem::size_of_val(&exclusive) as i32,
            )
        } != 0
        {
            return Err(std::io::Error::from_raw_os_error(unsafe {
                WSAGetLastError()
            }));
        }
    }
    socket.bind(SocketAddr::from((address, port)))?;
    socket.listen(128)
}

pub async fn start_on(
    identity: &Identity,
    address: Ipv4Addr,
    port: u16,
    engine: Arc<Engine>,
) -> ServiceHandle {
    let snapshot = Arc::new(RwLock::new(ServiceSnapshot {
        state: ServiceState::Error,
        address: address.to_string(),
        port,
        error: None,
        scope: ServiceScope::Lan,
    }));
    let mut handle = ServiceHandle {
        snapshot: snapshot.clone(),
        stop: None,
        task: None,
    };
    let listener_and_tls = async {
        let config = tls::server(identity)?;
        let listener = bind_listener(address, port).map_err(|error| {
            format!("无法在 {address}:{port} 启动 TLS 服务（端口可能被占用）：{error}")
        })?;
        let actual_port = listener
            .local_addr()
            .map_err(|error| format!("无法读取监听地址：{error}"))?
            .port();
        Ok::<_, String>((listener, TlsAcceptor::from(Arc::new(config)), actual_port))
    }
    .await;
    match listener_and_tls {
        Ok((listener, acceptor, actual_port)) => {
            if let Ok(mut state) = snapshot.write() {
                state.state = ServiceState::Running;
                state.port = actual_port;
            }
            engine.set_port(actual_port);
            let (stop, receiver) = oneshot::channel();
            handle.stop = Some(stop);
            handle.task = Some(tokio::spawn(serve(
                listener, acceptor, receiver, snapshot, engine,
            )));
        }
        Err(error) => {
            if let Ok(mut state) = snapshot.write() {
                state.error = Some(error);
            }
        }
    }
    handle
}

async fn serve(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    mut stop: oneshot::Receiver<()>,
    snapshot: Arc<RwLock<ServiceSnapshot>>,
    engine: Arc<Engine>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, remote) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        if let Ok(mut state) = snapshot.write() {
                            state.state = ServiceState::Error;
                            state.error = Some(format!("本机 TLS 服务接受连接失败：{error}"));
                        }
                        break;
                    }
                };
                if !matches!(remote.ip(), std::net::IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || ip.is_link_local()) {
                    continue;
                }
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                let acceptor = acceptor.clone();
                let engine = engine.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(tls)) = timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await else { return };
                    let Some(certificates) = tls.get_ref().1.peer_certificates() else { return };
                    if certificates.len() != 1 { return }
                    let fingerprint = identity::fingerprint(certificates[0].as_ref());
                    let io = TokioIo::new(tls);
                    let mut http = http1::Builder::new();
                    http.keep_alive(false).max_buf_size(8192).max_headers(32)
                        .timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
                    let connection = http.serve_connection(io, service_fn(move |request| {
                        let engine = engine.clone();
                        let fingerprint = fingerprint.clone();
                        async move {
                            if request.uri().path() == "/health" {
                                health(request, remote).await
                            } else {
                                engine.handle(request, fingerprint, remote).await
                            }
                        }
                    }));
                    let _ = connection.await;
                });
            }
        }
    }
    drop(listener);
    if timeout(Duration::from_secs(30), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

async fn health(
    request: Request<Incoming>,
    remote: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let has_body = request.headers().contains_key(TRANSFER_ENCODING)
        || request
            .headers()
            .get(CONTENT_LENGTH)
            .is_some_and(|value| value != "0");
    let (status, body) = if !remote.ip().is_loopback() || request.uri().query().is_some() {
        (StatusCode::NOT_FOUND, "{\"error\":\"not_found\"}")
    } else if request.method() != Method::GET {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            "{\"error\":\"method_not_allowed\"}",
        )
    } else if has_body {
        (StatusCode::BAD_REQUEST, "{\"error\":\"body_not_allowed\"}")
    } else {
        (StatusCode::OK, HEALTH_BODY)
    };
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HealthResponse {
    version: String,
    status: String,
}

pub async fn probe(identity: &Identity, port: u16) -> Result<String, String> {
    let client = tls::client(
        identity,
        &identity.fingerprint,
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    )?;
    let mut response = client
        .get(format!("https://localhost:{port}/health"))
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|error| format!("本机双向 TLS 健康请求失败（固定证书验证）：{error}"))?;
    if response.status() != StatusCode::OK {
        return Err(format!("本机健康服务返回异常状态：{}", response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取健康响应失败：{error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_HEALTH_BYTES {
            return Err("健康响应超过 1 KiB 限制。".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let health: HealthResponse =
        serde_json::from_slice(&bytes).map_err(|_| "健康服务未返回预期的 JSON 响应。")?;
    if health.status != "ok" || health.version != env!("CARGO_PKG_VERSION") {
        return Err("健康服务状态或版本与当前应用不匹配。".into());
    }
    Ok(format!(
        "本机端口 {port} 已通过真实双向 TLS 健康请求、完整证书指纹和证书签名验证，版本 {}。",
        health.version
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_mutual_tls_rejects_changed_identity_and_anonymous_client() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Arc::new(identity::generate().unwrap());
        let engine =
            Engine::new(directory.path().to_owned(), identity.clone(), "测试".into()).unwrap();
        let service = start_on(&identity, Ipv4Addr::LOCALHOST, 0, engine.clone()).await;
        let port = service.snapshot().port;
        assert_eq!(service.snapshot().state, ServiceState::Running);
        assert!(probe(&identity, port).await.is_ok());
        let stranger = identity::generate().unwrap();
        let client = tls::client(
            &stranger,
            &stranger.fingerprint,
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        )
        .unwrap();
        assert!(client
            .get(format!("https://localhost:{port}/health"))
            .send()
            .await
            .is_err());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(identity.certificate.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let anonymous = reqwest::Client::builder()
            .use_preconfigured_tls(config)
            .no_proxy()
            .build()
            .unwrap();
        assert!(anonymous
            .get(format!("https://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_err());
        let client = tls::client(
            &stranger,
            &identity.fingerprint,
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        )
        .unwrap();
        let response = client
            .post(format!("https://localhost:{port}/v1/offers"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert!(!response.status().is_success());
        assert_eq!(
            client
                .get(format!("https://localhost:{port}/health"))
                .body("reject")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        engine.shutdown().await;
        service.shutdown().await;
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.is_ok());
    }

    #[tokio::test]
    async fn port_conflict_is_reported_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Arc::new(identity::generate().unwrap());
        let engine =
            Engine::new(directory.path().to_owned(), identity.clone(), "测试".into()).unwrap();
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let service = start(
            &identity,
            occupied.local_addr().unwrap().port(),
            engine.clone(),
        )
        .await;
        assert_eq!(service.snapshot().state, ServiceState::Error);
        assert!(service.snapshot().error.unwrap().contains("端口可能被占用"));
        engine.shutdown().await;
        service.shutdown().await;
    }
}
