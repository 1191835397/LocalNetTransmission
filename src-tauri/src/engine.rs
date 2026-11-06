//! P2 protocol and session state. `new`, `respond_pair`, and `forget_peer` perform
//! durable trust-file IO: synchronous IPC callers must run them in spawn_blocking.
//! All content IO, hashing, and commits are performed by tracked blocking workers.
use crate::{identity::Identity, storage};
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use futures_util::stream;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::mpsc, time::timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const BLOCK: usize = 64 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024 * 1024;
const MAX_JSON: usize = 4096;
const MAX_PEERS: usize = 64;
const MAX_TASKS: usize = 128;
const PAIR_TTL: Duration = Duration::from_secs(120);
const GRANT_TTL: Duration = Duration::from_secs(300);
const IDLE: Duration = Duration::from_secs(30);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerInfo {
    pub fingerprint: String,
    pub name: String,
    pub address: String,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequestInfo {
    pub id: String,
    pub fingerprint: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub expires_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferInfo {
    pub id: String,
    pub direction: String,
    pub peer_fingerprint: String,
    pub peer_name: String,
    pub file_name: String,
    pub size: u64,
    pub bytes_transferred: u64,
    pub bytes_per_second: f64,
    pub status: String,
    pub error: Option<String>,
    pub saved_path: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferSnapshot {
    pub peers: Vec<PeerInfo>,
    pub pair_requests: Vec<PairRequestInfo>,
    pub tasks: Vec<TransferInfo>,
}

#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    version: u8,
    id: String,
    file_id: String,
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairHello {
    version: u8,
    name: String,
    port: u16,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairReply {
    version: u8,
    id: String,
    status: String,
    name: String,
    port: u16,
}

// Deliberately not TransferInfo: local directories and saved paths never go on wire.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransferReply {
    version: u8,
    id: String,
    file_id: String,
    name: String,
    size: u64,
    sha256: String,
    status: String,
    token: Option<String>,
    error: Option<String>,
}

struct Pair {
    info: PairRequestInfo,
    deadline: Instant,
    status: &'static str,
}

struct Task {
    info: TransferInfo,
    manifest: Option<Manifest>,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    token: Option<String>,
    claimed: bool,
    directory: Option<PathBuf>,
    started: Instant,
    running: bool,
    upload_started: bool,
}

impl Task {
    fn new(id: String, peer: &PeerInfo, direction: &str, name: String, size: u64) -> Self {
        Self {
            info: TransferInfo {
                id,
                direction: direction.into(),
                peer_fingerprint: peer.fingerprint.clone(),
                peer_name: peer.name.clone(),
                file_name: name,
                size,
                bytes_transferred: 0,
                bytes_per_second: 0.0,
                status: if direction == "send" {
                    "hashing"
                } else {
                    "awaitingAcceptance"
                }
                .into(),
                error: None,
                saved_path: None,
            },
            manifest: None,
            cancel: CancellationToken::new(),
            deadline: None,
            token: None,
            claimed: false,
            directory: None,
            started: Instant::now(),
            running: false,
            upload_started: false,
        }
    }

    fn finish(&mut self, status: &str, error: Option<&str>) {
        if terminal(&self.info.status) {
            return;
        }
        self.info.status = status.into();
        self.info.error = error.map(str::to_owned);
        self.info.bytes_per_second = 0.0;
        self.deadline = None;
        self.token = None;
        if terminal(status) {
            self.cancel.cancel();
        }
    }

    fn reply(&self) -> Result<TransferReply, String> {
        let m = self.manifest.as_ref().ok_or("manifest_unavailable")?;
        Ok(TransferReply {
            version: 1,
            id: m.id.clone(),
            file_id: m.file_id.clone(),
            name: m.name.clone(),
            size: m.size,
            sha256: m.sha256.clone(),
            status: self.info.status.clone(),
            token: if self.info.status == "ready" && !self.claimed {
                self.token.clone()
            } else {
                None
            },
            error: self.info.error.clone(),
        })
    }
}

struct State {
    name: String,
    port: u16,
    peers: BTreeMap<String, PeerInfo>,
    pairs: BTreeMap<String, Pair>,
    tasks: BTreeMap<String, Task>,
    pair_attempts: VecDeque<Instant>,
    stopping: bool,
    connecting: bool,
    trust_epoch: u64,
}

impl State {
    fn expire(&mut self) {
        let now = Instant::now();
        self.pairs.retain(|_, pair| pair.deadline > now);
        while self
            .pair_attempts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= PAIR_TTL)
        {
            self.pair_attempts.pop_front();
        }
        for task in self.tasks.values_mut() {
            if task.deadline.is_some_and(|deadline| deadline <= now) && !terminal(&task.info.status)
            {
                task.cancel.cancel();
                task.finish("failed", Some("acceptance_expired"));
            }
        }
    }

    fn available(&self, direction: &str) -> Result<(), String> {
        if self.stopping {
            return Err("shutting_down".into());
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err("history_full".into());
        }
        if self
            .tasks
            .values()
            .any(|t| t.info.direction == direction && (t.running || !terminal(&t.info.status)))
        {
            return Err("direction_busy".into());
        }
        Ok(())
    }

    fn check_peer(&self, peer: &PeerInfo) -> Result<(), String> {
        if self.stopping {
            return Err("shutting_down".into());
        }
        if self.peers.values().any(|p| {
            p.address == peer.address && p.port == peer.port && p.fingerprint != peer.fingerprint
        }) {
            return Err("endpoint_identity_changed_revoke_first".into());
        }
        if let Some(old) = self.peers.get(&peer.fingerprint) {
            if old.address != peer.address || old.port != peer.port {
                return Err("peer_endpoint_changed_revoke_first".into());
            }
        } else if self.peers.len() >= MAX_PEERS {
            return Err("peers_full".into());
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustDocument {
    version: u8,
    peers: Vec<PeerInfo>,
}

struct TrustStore {
    directory: PathBuf,
    _lock: File,
    baseline: Mutex<Option<Vec<u8>>>,
}

impl TrustStore {
    fn open(directory: PathBuf) -> Result<(Self, BTreeMap<String, PeerInfo>), String> {
        if !directory.is_absolute() || !directory.is_dir() {
            return Err("invalid_config_directory".into());
        }
        let lock_path = directory.join("peers.lock");
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
            if !meta.is_file() || special_file(&meta) {
                return Err("unsafe_trust_lock".into());
            }
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| "trust_lock_unavailable")?;
        lock.try_lock().map_err(|_| "trust_store_in_use")?;
        let bytes = Self::read(&directory)?;
        let peers = match &bytes {
            Some(bytes) => parse_trust(bytes)?,
            None => BTreeMap::new(),
        };
        Ok((
            Self {
                directory,
                _lock: lock,
                baseline: Mutex::new(bytes),
            },
            peers,
        ))
    }

    fn read(directory: &Path) -> Result<Option<Vec<u8>>, String> {
        let path = directory.join("peers.json");
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("trust_read_failed".into()),
            Ok(meta) if !meta.is_file() || special_file(&meta) => {
                return Err("unsafe_trust_file".into())
            }
            Ok(_) => {}
        }
        let file = storage::open_source(&path).map_err(|_| "trust_read_failed")?;
        let mut bytes = Vec::new();
        file.take(BLOCK as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "trust_read_failed")?;
        if bytes.len() > BLOCK {
            return Err("trust_file_too_large".into());
        }
        Ok(Some(bytes))
    }

    fn persist(&self, peers: &BTreeMap<String, PeerInfo>) -> Result<(), String> {
        let mut baseline = self.baseline.lock().map_err(|_| "trust_lock_poisoned")?;
        // The lifetime OS lock excludes other engines. Detect non-cooperating edits too;
        // never silently replace a damaged or externally modified trust document.
        if Self::read(&self.directory)? != *baseline {
            return Err("trust_file_changed_restart_required".into());
        }
        let document = TrustDocument {
            version: 1,
            peers: peers.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| "trust_encode_failed")?;
        parse_trust(&bytes)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".lanlink-peers-")
            .tempfile_in(&self.directory)
            .map_err(|_| "trust_write_failed")?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| "trust_sync_failed")?;
        temporary
            .persist(self.directory.join("peers.json"))
            .map_err(|_| "trust_commit_failed")?;
        *baseline = Some(bytes);
        Ok(())
    }
}

#[derive(Clone)]
pub struct Engine {
    identity: Arc<Identity>,
    state: Arc<Mutex<State>>,
    store: Arc<TrustStore>,
    jobs: TaskTracker,
    stop: CancellationToken,
    runtime: Handle,
}

impl Engine {
    pub fn new(
        config_dir: PathBuf,
        identity: Arc<Identity>,
        name: String,
    ) -> Result<Arc<Self>, String> {
        let name = valid_name(&name)?;
        let runtime = Handle::try_current().map_err(|_| "runtime_unavailable")?;
        let (store, peers) = TrustStore::open(config_dir)?;
        if peers.contains_key(&identity.fingerprint) {
            return Err("self_trust_not_allowed".into());
        }
        let engine = Arc::new(Self {
            identity,
            state: Arc::new(Mutex::new(State {
                name,
                port: 0,
                peers,
                pairs: BTreeMap::new(),
                tasks: BTreeMap::new(),
                pair_attempts: VecDeque::new(),
                stopping: false,
                connecting: false,
                trust_epoch: 0,
            })),
            store: Arc::new(store),
            jobs: TaskTracker::new(),
            stop: CancellationToken::new(),
            runtime,
        });
        let weak = Arc::downgrade(&engine.state);
        let stop = engine.stop.clone();
        engine.jobs.spawn_on(
            async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {
                            let Some(state) = weak.upgrade() else { break; };
                            if let Ok(mut state) = state.lock() { state.expire(); };
                        }
                    }
                }
            },
            &engine.runtime,
        );
        Ok(engine)
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "engine_state_poisoned".into())
    }

    pub fn set_name(&self, name: String) {
        if let Ok(name) = valid_name(&name) {
            if let Ok(mut state) = self.lock() {
                state.name = name;
            }
        }
    }

    pub fn set_port(&self, port: u16) {
        if port >= 1024 {
            if let Ok(mut state) = self.lock() {
                state.port = port;
            }
        }
    }

    pub fn snapshot(&self) -> Result<TransferSnapshot, String> {
        let mut state = self.lock()?;
        state.expire();
        Ok(TransferSnapshot {
            peers: state.peers.values().cloned().collect(),
            pair_requests: state
                .pairs
                .values()
                .filter(|p| p.status == "pending")
                .map(|p| p.info.clone())
                .collect(),
            tasks: state.tasks.values().map(|t| t.info.clone()).collect(),
        })
    }

    pub fn has_active(&self) -> bool {
        self.lock()
            .map(|mut s| {
                s.expire();
                s.connecting
                    || s.pairs.values().any(|p| p.status == "pending")
                    || s.tasks
                        .values()
                        .any(|t| t.running || !terminal(&t.info.status))
            })
            .unwrap_or(true)
    }

    pub async fn connect(
        self: &Arc<Self>,
        address: String,
        port: u16,
        fingerprint: String,
    ) -> Result<PeerInfo, String> {
        let address = endpoint(&address, port)?;
        let fingerprint = normalize_fingerprint(&fingerprint)?;
        if fingerprint == self.identity.fingerprint {
            return Err("self_pair_not_allowed".into());
        }
        let job = {
            let mut state = self.lock()?;
            state.expire();
            if state.connecting {
                return Err("pair_connect_busy".into());
            }
            let peer = PeerInfo {
                fingerprint: fingerprint.clone(),
                address: address.ip().to_string(),
                port,
                name: "pending".into(),
            };
            state.check_peer(&peer)?;
            if state.port < 1024 {
                return Err("listener_not_ready".into());
            }
            state.connecting = true;
            let epoch = state.trust_epoch;
            let engine = self.as_ref().clone();
            self.jobs.spawn_on(
                async move {
                    let result = engine.connect_inner(address, fingerprint, epoch).await;
                    if let Ok(mut s) = engine.lock() {
                        s.connecting = false;
                    }
                    result
                },
                &self.runtime,
            )
        };
        job.await.map_err(|_| "pair_worker_failed")?
    }

    async fn connect_inner(
        &self,
        address: SocketAddr,
        fingerprint: String,
        epoch: u64,
    ) -> Result<PeerInfo, String> {
        let client = crate::tls::client(&self.identity, &fingerprint, address)?;
        let hello = {
            let s = self.lock()?;
            PairHello {
                version: 1,
                name: s.name.clone(),
                port: s.port,
            }
        };
        let mut reply: PairReply = control(
            client.post(url(address, "/v1/pair")).json(&hello),
            &self.stop,
        )
        .await?;
        let deadline = Instant::now() + PAIR_TTL;
        let id = reply.id.clone();
        if !valid_id(&id) {
            return Err("invalid_pair_response".into());
        }
        loop {
            if reply.version != 1 || reply.id != id || reply.port != address.port() {
                return Err("invalid_pair_response".into());
            }
            let name = valid_name(&reply.name)?;
            match reply.status.as_str() {
                "approved" => {
                    let peer = PeerInfo {
                        fingerprint,
                        name,
                        address: address.ip().to_string(),
                        port: address.port(),
                    };
                    let engine = self.clone();
                    return self
                        .jobs
                        .spawn_blocking_on(
                            move || {
                                let mut state = engine.lock()?;
                                if state.trust_epoch != epoch {
                                    return Err("trust_changed_retry_pair".into());
                                }
                                state.check_peer(&peer)?;
                                let mut peers = state.peers.clone();
                                peers.insert(peer.fingerprint.clone(), peer.clone());
                                engine.store.persist(&peers)?;
                                state.peers = peers;
                                Ok(peer)
                            },
                            &self.runtime,
                        )
                        .await
                        .map_err(|_| "trust_worker_failed")?;
                }
                "pending" => {}
                "rejected" => return Err("pair_rejected".into()),
                _ => return Err("invalid_pair_response".into()),
            }
            if Instant::now() >= deadline {
                return Err("pair_expired".into());
            }
            pause(&self.stop).await?;
            reply = control(
                client.get(url(address, &format!("/v1/pair/{id}"))),
                &self.stop,
            )
            .await?;
        }
    }

    /// Durable synchronous operation; invoke from a blocking IPC worker.
    pub fn respond_pair(&self, request_id: String, accept: bool) -> Result<(), String> {
        let mut state = self.lock()?;
        state.expire();
        if state.stopping {
            return Err("shutting_down".into());
        }
        let pair = state
            .pairs
            .get(&request_id)
            .ok_or("pair_not_found_or_expired")?;
        if pair.status != "pending" {
            return Err("pair_already_decided".into());
        }
        let peer = PeerInfo {
            fingerprint: pair.info.fingerprint.clone(),
            name: pair.info.name.clone(),
            address: pair.info.address.clone(),
            port: pair.info.port,
        };
        if accept {
            state.check_peer(&peer)?;
            let mut peers = state.peers.clone();
            peers.insert(peer.fingerprint.clone(), peer);
            self.store.persist(&peers)?;
            state.peers = peers;
        }
        state
            .pairs
            .get_mut(&request_id)
            .ok_or("pair_not_found")?
            .status = if accept { "approved" } else { "rejected" };
        Ok(())
    }

    /// Durable synchronous operation; invoke from a blocking IPC worker.
    pub fn forget_peer(&self, fingerprint: String) -> Result<(), String> {
        let fingerprint = normalize_fingerprint(&fingerprint)?;
        let mut state = self.lock()?;
        if state.stopping {
            return Err("shutting_down".into());
        }
        let mut peers = state.peers.clone();
        peers.remove(&fingerprint);
        // Persist first. An unsuccessful revoke is reported, never falsely acknowledged.
        self.store.persist(&peers)?;
        state.peers = peers;
        state.trust_epoch = state.trust_epoch.wrapping_add(1);
        state
            .pairs
            .retain(|_, pair| pair.info.fingerprint != fingerprint);
        for task in state
            .tasks
            .values_mut()
            .filter(|t| t.info.peer_fingerprint == fingerprint)
        {
            task.cancel.cancel();
            if task.info.direction == "send" && task.upload_started {
                task.finish("unknown", Some("peer_revoked_result_unknown"));
            } else {
                task.finish("cancelled", Some("peer_revoked"));
            }
        }
        Ok(())
    }

    pub fn send(self: &Arc<Self>, fingerprint: String, path: String) -> Result<String, String> {
        let fingerprint = normalize_fingerprint(&fingerprint)?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("source_must_be_absolute".into());
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid_file_name")?
            .to_owned();
        storage::validate_name(&name).map_err(|_| "invalid_file_name")?;
        let id = random_id()?;
        let file_id = random_id()?;
        let mut state = self.lock()?;
        state.expire();
        state.available("send")?;
        let peer = state
            .peers
            .get(&fingerprint)
            .ok_or("untrusted_peer")?
            .clone();
        let mut task = Task::new(id.clone(), &peer, "send", name, 0);
        task.running = true;
        let cancel = task.cancel.clone();
        state.tasks.insert(id.clone(), task);
        let engine = self.as_ref().clone();
        let task_id = id.clone();
        self.jobs.spawn_on(
            async move {
                let result = engine
                    .send_inner(&task_id, file_id, path, peer, cancel)
                    .await;
                if let Ok(mut s) = engine.lock() {
                    if let Some(task) = s.tasks.get_mut(&task_id) {
                        if let Err(error) = result {
                            let status = if task.upload_started {
                                "unknown"
                            } else {
                                "failed"
                            };
                            task.finish(status, Some(&error));
                        }
                        task.running = false;
                    }
                }
            },
            &self.runtime,
        );
        Ok(id)
    }

    pub fn respond_transfer(
        &self,
        task_id: String,
        accept: bool,
        directory: Option<String>,
    ) -> Result<(), String> {
        let mut state = self.lock()?;
        state.expire();
        if state.stopping {
            return Err("shutting_down".into());
        }
        let task = state.tasks.get_mut(&task_id).ok_or("task_not_found")?;
        if task.info.direction != "receive" || task.info.status != "awaitingAcceptance" {
            return Err("transfer_already_decided".into());
        }
        if !accept {
            task.finish("rejected", None);
            return Ok(());
        }
        let directory = PathBuf::from(directory.ok_or("receive_directory_required")?);
        if !directory.is_absolute() {
            return Err("receive_directory_must_be_absolute".into());
        }
        task.token = Some(random_id()?);
        task.directory = Some(directory);
        task.deadline = Some(Instant::now() + GRANT_TTL);
        task.info.status = "ready".into();
        Ok(())
    }

    pub fn cancel(&self, task_id: String) -> Result<(), String> {
        let mut state = self.lock()?;
        if state.stopping {
            return Err("shutting_down".into());
        }
        let task = state.tasks.get_mut(&task_id).ok_or("task_not_found")?;
        if terminal(&task.info.status) {
            return Ok(());
        }
        task.cancel.cancel();
        let remote = task.info.direction == "send";
        let fingerprint = task.info.peer_fingerprint.clone();
        if remote && task.upload_started {
            task.finish("unknown", Some("cancellation_pending_remote_result"));
        } else {
            task.finish("cancelled", None);
        }
        if remote {
            if let Some(peer) = state.peers.get(&fingerprint).cloned() {
                let engine = self.clone();
                self.jobs.spawn_on(
                    async move {
                        if let Ok((client, address)) = engine.peer_client(&peer) {
                            if let Ok(reply) = control::<TransferReply>(
                                client
                                    .post(url(address, &format!("/v1/transfers/{task_id}/cancel"))),
                                &engine.stop,
                            )
                            .await
                            {
                                let _ = engine.apply_remote(&task_id, reply);
                            }
                        }
                    },
                    &self.runtime,
                );
            }
        }
        Ok(())
    }

    pub async fn refresh_result(self: &Arc<Self>, task_id: String) -> Result<(), String> {
        let job = {
            let state = self.lock()?;
            if state.stopping {
                return Err("shutting_down".into());
            }
            let task = state.tasks.get(&task_id).ok_or("task_not_found")?;
            if task.info.direction != "send" {
                return Err("not_a_send_task".into());
            }
            if terminal(&task.info.status) {
                return Ok(());
            }
            if task.info.status != "unknown" {
                return Err("result_not_unknown".into());
            }
            let peer = state
                .peers
                .get(&task.info.peer_fingerprint)
                .ok_or("untrusted_peer")?
                .clone();
            let engine = self.as_ref().clone();
            self.jobs.spawn_on(
                async move { engine.query_result(&task_id, &peer).await },
                &self.runtime,
            )
        };
        job.await.map_err(|_| "result_worker_failed")?
    }

    pub async fn shutdown(&self) {
        {
            let mut state = match self.state.lock() {
                Ok(s) => s,
                Err(e) => e.into_inner(),
            };
            state.stopping = true;
            self.stop.cancel();
            state.pairs.clear();
            for task in state.tasks.values_mut() {
                task.cancel.cancel();
                if task.info.direction == "send" && task.upload_started {
                    task.finish("unknown", Some("shutdown_result_unknown"));
                } else {
                    task.finish("cancelled", Some("shutting_down"));
                }
            }
            self.jobs.close();
        }
        // No aborts: blocking producers/writers cooperate, release descriptors, and
        // remove unfinished destinations before this future resolves.
        self.jobs.wait().await;
    }

    pub async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
        fingerprint: String,
        remote: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let fingerprint = match normalize_fingerprint(&fingerprint) {
            Ok(fp) => fp,
            Err(_) => return Ok(failure("invalid_identity")),
        };
        if !allowed_ip(remote.ip()) {
            return Ok(failure("invalid_address"));
        }
        let job = {
            let state = match self.lock() {
                Ok(s) => s,
                Err(_) => return Ok(failure("engine_state_poisoned")),
            };
            if state.stopping {
                return Ok(failure("shutting_down"));
            }
            let engine = self.as_ref().clone();
            self.jobs.spawn_on(
                async move {
                    match engine.dispatch(request, &fingerprint, remote).await {
                        Ok(r) => r,
                        Err(e) => failure(&e),
                    }
                },
                &self.runtime,
            )
        };
        Ok(job
            .await
            .unwrap_or_else(|_| failure("request_worker_failed")))
    }

    async fn dispatch(
        &self,
        request: Request<Incoming>,
        fingerprint: &str,
        remote: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, String> {
        if request.uri().query().is_some() {
            return Err("invalid_route".into());
        }
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        if path == "/v1/pair" && method == Method::POST {
            let hello: PairHello = request_json(request, &self.stop).await?;
            return json_response(&self.receive_pair(hello, fingerprint, remote)?);
        }
        if let Some(id) = path.strip_prefix("/v1/pair/") {
            if method != Method::GET || !valid_id(id) {
                return Err("invalid_route".into());
            }
            no_body(&request)?;
            let mut state = self.lock()?;
            state.expire();
            let pair = state
                .pairs
                .get(id)
                .filter(|p| p.info.fingerprint == fingerprint)
                .ok_or("pair_not_found_or_expired")?;
            return json_response(&PairReply {
                version: 1,
                id: id.into(),
                status: pair.status.into(),
                name: state.name.clone(),
                port: state.port,
            });
        }
        {
            let state = self.lock()?;
            if state.stopping {
                return Err("shutting_down".into());
            }
            if !state.peers.contains_key(fingerprint) {
                return Err("untrusted_peer".into());
            }
        }
        if path == "/v1/offers" && method == Method::POST {
            let manifest: Manifest = request_json(request, &self.stop).await?;
            return json_response(&self.offer(fingerprint, manifest)?);
        }
        let suffix = path.strip_prefix("/v1/transfers/").ok_or("not_found")?;
        let (id, action) = suffix.split_once('/').unwrap_or((suffix, ""));
        if !valid_id(id) {
            return Err("invalid_task_id".into());
        }
        match (method, action) {
            (Method::GET, "") => {
                no_body(&request)?;
                json_response(&self.transfer_status(fingerprint, id)?)
            }
            (Method::POST, "cancel") => {
                no_body(&request)?;
                let mut state = self.lock()?;
                if !state.peers.contains_key(fingerprint) {
                    return Err("untrusted_peer".into());
                }
                let task = owned_task(&mut state, fingerprint, id)?;
                task.cancel.cancel();
                task.finish("cancelled", None);
                json_response(&task.reply()?)
            }
            (Method::PUT, "content") => self.receive_content(request, fingerprint, id).await,
            _ => Err("not_found".into()),
        }
    }

    fn receive_pair(
        &self,
        hello: PairHello,
        fingerprint: &str,
        remote: SocketAddr,
    ) -> Result<PairReply, String> {
        if hello.version != 1 {
            return Err("unsupported_version".into());
        }
        if fingerprint == self.identity.fingerprint {
            return Err("self_pair_not_allowed".into());
        }
        let name = valid_name(&hello.name)?;
        let address = endpoint(&remote.ip().to_string(), hello.port)?;
        let peer = PeerInfo {
            fingerprint: fingerprint.into(),
            name: name.clone(),
            address: address.ip().to_string(),
            port: hello.port,
        };
        let mut state = self.lock()?;
        state.expire();
        state.check_peer(&peer)?;
        if state.port < 1024 {
            return Err("listener_not_ready".into());
        }
        if let Some(pair) = state
            .pairs
            .values()
            .find(|p| p.info.fingerprint == fingerprint)
        {
            if pair.info.address != peer.address
                || pair.info.port != peer.port
                || pair.info.name != peer.name
            {
                return Err("pair_manifest_changed".into());
            }
            return Ok(PairReply {
                version: 1,
                id: pair.info.id.clone(),
                status: pair.status.into(),
                name: state.name.clone(),
                port: state.port,
            });
        }
        if state.pair_attempts.len() >= 16 || state.pairs.len() >= 32 {
            return Err("pair_rate_limited".into());
        }
        state.pair_attempts.push_back(Instant::now());
        let trusted = state.peers.contains_key(fingerprint);
        if !trusted
            && state
                .pairs
                .values()
                .filter(|p| p.status == "pending")
                .count()
                >= 8
        {
            return Err("pair_requests_full".into());
        }
        if state.pairs.values().any(|p| {
            p.status != "rejected"
                && p.info.address == peer.address
                && p.info.port == peer.port
                && p.info.fingerprint != fingerprint
        }) {
            return Err("endpoint_pair_pending".into());
        }
        let id = random_id()?;
        let status = if trusted { "approved" } else { "pending" };
        state.pairs.insert(
            id.clone(),
            Pair {
                info: PairRequestInfo {
                    id: id.clone(),
                    fingerprint: fingerprint.into(),
                    name,
                    address: peer.address,
                    port: peer.port,
                    expires_at: (Utc::now() + chrono::Duration::seconds(120))
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                },
                deadline: Instant::now() + PAIR_TTL,
                status,
            },
        );
        Ok(PairReply {
            version: 1,
            id,
            status: status.into(),
            name: state.name.clone(),
            port: state.port,
        })
    }

    fn offer(&self, fingerprint: &str, manifest: Manifest) -> Result<TransferReply, String> {
        validate_manifest(&manifest)?;
        let mut state = self.lock()?;
        state.expire();
        if state.stopping {
            return Err("shutting_down".into());
        }
        let peer = state
            .peers
            .get(fingerprint)
            .ok_or("untrusted_peer")?
            .clone();
        if let Some(task) = state.tasks.get(&manifest.id) {
            if task.info.direction != "receive"
                || task.info.peer_fingerprint != fingerprint
                || task.manifest.as_ref() != Some(&manifest)
            {
                return Err("manifest_conflict".into());
            }
            return task.reply();
        }
        state.available("receive")?;
        let mut task = Task::new(
            manifest.id.clone(),
            &peer,
            "receive",
            manifest.name.clone(),
            manifest.size,
        );
        task.deadline = Some(Instant::now() + PAIR_TTL);
        task.manifest = Some(manifest.clone());
        let reply = task.reply()?;
        state.tasks.insert(manifest.id, task);
        Ok(reply)
    }

    fn transfer_status(&self, fingerprint: &str, id: &str) -> Result<TransferReply, String> {
        let mut state = self.lock()?;
        state.expire();
        if !state.peers.contains_key(fingerprint) {
            return Err("untrusted_peer".into());
        }
        owned_task(&mut state, fingerprint, id)?.reply()
    }

    fn claim(
        &self,
        fingerprint: &str,
        id: &str,
        headers: &hyper::HeaderMap,
    ) -> Result<(Manifest, PathBuf, CancellationToken), String> {
        let mut state = self.lock()?;
        state.expire();
        if state.stopping {
            return Err("shutting_down".into());
        }
        if !state.peers.contains_key(fingerprint) {
            return Err("untrusted_peer".into());
        }
        let task = owned_task(&mut state, fingerprint, id)?;
        if task.info.status != "ready" || task.claimed {
            return Err("grant_unavailable".into());
        }
        let manifest = task.manifest.as_ref().ok_or("manifest_unavailable")?;
        if headers.contains_key(header::TRANSFER_ENCODING)
            || headers.contains_key(header::CONTENT_ENCODING)
        {
            return Err("encoding_not_allowed".into());
        }
        if single_header(headers, "content-length")? != manifest.size.to_string() {
            return Err("length_mismatch".into());
        }
        if single_header(headers, "x-offset")? != "0"
            || single_header(headers, "x-file-id")? != manifest.file_id
        {
            return Err("file_binding_mismatch".into());
        }
        let authorization = single_header(headers, "authorization")?;
        let supplied = authorization
            .strip_prefix("Bearer ")
            .ok_or("invalid_grant")?;
        let expected = task.token.as_deref().ok_or("grant_unavailable")?;
        if !constant_equal(expected.as_bytes(), supplied.as_bytes()) {
            return Err("invalid_grant".into());
        }
        let directory = task.directory.clone().ok_or("receive_directory_required")?;
        task.claimed = true;
        task.token = None;
        task.deadline = None;
        task.info.status = "transferring".into();
        task.started = Instant::now();
        task.running = true;
        Ok((manifest.clone(), directory, task.cancel.clone()))
    }

    async fn receive_content(
        &self,
        request: Request<Incoming>,
        fingerprint: &str,
        id: &str,
    ) -> Result<Response<Full<Bytes>>, String> {
        let (manifest, directory, cancel) = self.claim(fingerprint, id, request.headers())?;
        let (tx, rx) = mpsc::channel(1);
        let engine = self.clone();
        let task_id = id.to_owned();
        let worker = self.jobs.spawn_blocking_on(
            move || {
                let result = engine.write_content(&task_id, manifest, directory, rx);
                if let Ok(mut state) = engine.lock() {
                    if let Some(task) = state.tasks.get_mut(&task_id) {
                        if let Err(error) = &result {
                            task.finish("failed", Some(error));
                        }
                        task.running = false;
                    }
                }
                result
            },
            &self.runtime,
        );
        let mut body = request.into_body();
        let mut ended = false;
        loop {
            let frame = tokio::select! {
                _ = self.stop.cancelled() => break,
                _ = cancel.cancelled() => break,
                result = timeout(IDLE, body.frame()) => result,
            };
            match frame {
                Ok(None) => {
                    ended = true;
                    break;
                }
                Ok(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else {
                        break;
                    };
                    let mut failed = false;
                    for chunk in data.chunks(BLOCK) {
                        let piece = Piece::Data(Bytes::copy_from_slice(chunk));
                        let result = tokio::select! {
                            _ = self.stop.cancelled() => { failed = true; break; },
                            _ = cancel.cancelled() => { failed = true; break; },
                            result = tx.send(piece) => result,
                        };
                        if result.is_err() {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                }
                _ => break,
            }
        }
        if ended {
            let _ = tx.send(Piece::End).await;
        }
        drop(tx);
        worker.await.map_err(|_| "receive_worker_failed")??;
        json_response(&self.transfer_status(fingerprint, id)?)
    }

    fn write_content(
        &self,
        id: &str,
        manifest: Manifest,
        directory: PathBuf,
        mut rx: mpsc::Receiver<Piece>,
    ) -> Result<(), String> {
        self.check_task_live(id)?;
        let mut destination = storage::Destination::new(&directory, &manifest.name)
            .map_err(|_| "destination_create_failed")?;
        destination.check_capacity(manifest.size)?;
        let mut digest = Sha256::new();
        let mut received = 0u64;
        loop {
            self.check_task_live(id)?;
            match rx.blocking_recv() {
                Some(Piece::Data(bytes)) => {
                    received = received
                        .checked_add(bytes.len() as u64)
                        .ok_or("length_mismatch")?;
                    if received > manifest.size || bytes.len() > BLOCK {
                        return Err("length_mismatch".into());
                    }
                    destination
                        .file_mut()
                        .write_all(&bytes)
                        .map_err(|_| "destination_write_failed")?;
                    digest.update(&bytes);
                    self.progress(id, received, false)?;
                }
                Some(Piece::End) => break,
                None => return Err("body_interrupted".into()),
            }
        }
        if received != manifest.size {
            return Err("length_mismatch".into());
        }
        {
            let mut state = self.lock()?;
            let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
            if task.cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            task.info.status = "verifying".into();
        }
        if hex(&digest.finalize()) != manifest.sha256 {
            return Err("digest_mismatch".into());
        }
        destination
            .file_mut()
            .sync_all()
            .map_err(|_| "destination_sync_failed")?;
        // Commit and terminal publication are one critical section. Cancellation and
        // revocation take this same lock; neither can publish a false cancellation
        // after a successful no-clobber commit. This runs on a blocking worker.
        let mut state = self.lock()?;
        if state.stopping || !state.peers.contains_key(&self.task_peer(&state, id)?) {
            return Err("cancelled".into());
        }
        let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
        if task.cancel.is_cancelled() || terminal(&task.info.status) {
            return Err("cancelled".into());
        }
        let path = destination
            .commit()
            .map_err(|_| "destination_commit_failed")?;
        task.info.saved_path = Some(path.to_string_lossy().into_owned());
        task.info.bytes_transferred = manifest.size;
        task.finish("completed", None);
        Ok(())
    }

    fn task_peer(&self, state: &State, id: &str) -> Result<String, String> {
        state
            .tasks
            .get(id)
            .map(|t| t.info.peer_fingerprint.clone())
            .ok_or_else(|| "task_not_found".into())
    }

    fn check_task_live(&self, id: &str) -> Result<(), String> {
        let state = self.lock()?;
        let task = state.tasks.get(id).ok_or("task_not_found")?;
        if state.stopping
            || task.cancel.is_cancelled()
            || terminal(&task.info.status)
            || !state.peers.contains_key(&task.info.peer_fingerprint)
        {
            return Err("cancelled".into());
        }
        Ok(())
    }

    fn progress(&self, id: &str, bytes: u64, hashing: bool) -> Result<(), String> {
        let mut state = self.lock()?;
        let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
        if task.cancel.is_cancelled() || terminal(&task.info.status) {
            return Err("cancelled".into());
        }
        task.info.bytes_transferred = bytes;
        task.info.bytes_per_second = if hashing {
            0.0
        } else {
            bytes as f64 / task.started.elapsed().as_secs_f64().max(0.001)
        };
        Ok(())
    }

    fn peer_client(&self, peer: &PeerInfo) -> Result<(reqwest::Client, SocketAddr), String> {
        let address = endpoint(&peer.address, peer.port)?;
        Ok((
            crate::tls::client(&self.identity, &peer.fingerprint, address)?,
            address,
        ))
    }

    fn hash_source(
        &self,
        id: &str,
        file_id: String,
        path: PathBuf,
    ) -> Result<(File, Manifest), String> {
        self.check_task_live(id)?;
        let mut file = storage::open_source(&path).map_err(|_| "source_open_failed")?;
        let size = file.metadata().map_err(|_| "source_metadata_failed")?.len();
        if size > MAX_FILE {
            return Err("file_too_large".into());
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("invalid_file_name")?
            .to_owned();
        storage::validate_name(&name).map_err(|_| "invalid_file_name")?;
        {
            let mut state = self.lock()?;
            state.tasks.get_mut(id).ok_or("task_not_found")?.info.size = size;
        }
        let mut digest = Sha256::new();
        let mut buffer = vec![0; BLOCK];
        let mut total = 0u64;
        loop {
            self.check_task_live(id)?;
            let count = file.read(&mut buffer).map_err(|_| "source_read_failed")?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > size {
                return Err("source_changed".into());
            }
            digest.update(&buffer[..count]);
            self.progress(id, total, true)?;
        }
        if total != size {
            return Err("source_changed".into());
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| "source_seek_failed")?;
        Ok((
            file,
            Manifest {
                version: 1,
                id: id.into(),
                file_id,
                name,
                size,
                sha256: hex(&digest.finalize()),
            },
        ))
    }

    async fn send_inner(
        &self,
        id: &str,
        file_id: String,
        path: PathBuf,
        peer: PeerInfo,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let engine = self.clone();
        let task_id = id.to_owned();
        let (file, manifest) = self
            .jobs
            .spawn_blocking_on(
                move || engine.hash_source(&task_id, file_id, path),
                &self.runtime,
            )
            .await
            .map_err(|_| "hash_worker_failed")??;
        self.check_task_live(id)?;
        {
            let mut state = self.lock()?;
            let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
            if task.cancel.is_cancelled() || terminal(&task.info.status) {
                return Err("cancelled".into());
            }
            task.manifest = Some(manifest.clone());
            task.info.status = "awaitingAcceptance".into();
            task.info.bytes_transferred = 0;
            task.deadline = Some(Instant::now() + PAIR_TTL);
        }
        let (client, address) = self.peer_client(&peer)?;
        let mut reply: TransferReply = control(
            client.post(url(address, "/v1/offers")).json(&manifest),
            &cancel,
        )
        .await?;
        let token = loop {
            validate_reply(&reply, &manifest)?;
            if terminal(&reply.status) {
                self.apply_remote(id, reply)?;
                return Ok(());
            }
            if reply.status == "ready" {
                break reply.token.filter(|t| valid_id(t)).ok_or("invalid_grant")?;
            }
            if reply.status != "awaitingAcceptance" {
                return Err("unexpected_remote_status".into());
            }
            self.check_task_live(id)?;
            pause(&cancel).await?;
            reply = control(
                client.get(url(address, &format!("/v1/transfers/{id}"))),
                &cancel,
            )
            .await?;
        };
        self.check_task_live(id)?;
        {
            let mut state = self.lock()?;
            let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
            if task.cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            task.info.status = "transferring".into();
            task.deadline = None;
            task.upload_started = true;
            task.started = Instant::now();
        }
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let engine = self.clone();
        let task_id = id.to_owned();
        let producer_manifest = manifest.clone();
        let producer = self.jobs.spawn_blocking_on(
            move || {
                let result = engine.produce(&task_id, file, &producer_manifest, &tx);
                if result.is_err() {
                    let _ = tx.blocking_send(Err(std::io::Error::other("source_stream_failed")));
                }
                result
            },
            &self.runtime,
        );
        let body = reqwest::Body::wrap_stream(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        }));
        let upload = client
            .put(url(address, &format!("/v1/transfers/{id}/content")))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("x-file-id", &manifest.file_id)
            .header("x-offset", "0")
            .header(header::CONTENT_LENGTH, manifest.size)
            .body(body)
            .send();
        // No whole-file timeout: large transfers are bounded by idle IO instead.
        // The watchdog observes producer progress and cancels stalled uploads.
        let upload_result = tokio::select! {
            _ = cancel.cancelled() => Err("cancelled".to_owned()),
            _ = self.stop.cancelled() => Err("shutting_down".to_owned()),
            _ = self.upload_watchdog(id) => Err("upload_idle_timeout".to_owned()),
            response = upload => match response {
                Ok(response) => response_json::<TransferReply>(response, &self.stop).await,
                Err(_) => Err("upload_acknowledgement_lost".to_owned()),
            }
        };
        // Dropping upload/body closes the bounded channel, unblocking the producer.
        let _ = producer.await.map_err(|_| "source_worker_failed")?;
        if let Ok(reply) = upload_result {
            if self.apply_remote(id, reply)? {
                return Ok(());
            }
        }
        {
            let mut state = self.lock()?;
            let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
            task.finish("unknown", Some("remote_result_unknown_refresh_required"));
        }
        // A lost PUT acknowledgement is not failure or success. Query the retained
        // owner-bound result before reporting any final outcome; never resend bytes.
        for _ in 0..3 {
            if self.query_result(id, &peer).await.is_ok() {
                let state = self.lock()?;
                if state
                    .tasks
                    .get(id)
                    .is_some_and(|t| terminal(&t.info.status))
                {
                    return Ok(());
                }
            }
            pause(&self.stop).await?;
        }
        Ok(())
    }

    async fn upload_watchdog(&self, id: &str) {
        let mut last_bytes = 0;
        let mut changed = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let bytes = self
                .lock()
                .ok()
                .and_then(|s| s.tasks.get(id).map(|t| t.info.bytes_transferred));
            match bytes {
                Some(bytes) if bytes != last_bytes => {
                    last_bytes = bytes;
                    changed = Instant::now();
                }
                None => return,
                _ => {}
            }
            if changed.elapsed() >= IDLE {
                return;
            }
        }
    }

    fn produce(
        &self,
        id: &str,
        mut file: File,
        manifest: &Manifest,
        tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    ) -> Result<(), String> {
        let mut sent = 0u64;
        let mut digest = Sha256::new();
        loop {
            self.check_task_live(id)?;
            let count = (manifest.size - sent).min(BLOCK as u64) as usize;
            let mut buffer = vec![0; count];
            file.read_exact(&mut buffer)
                .map_err(|_| "source_changed_or_unreadable")?;
            digest.update(&buffer);
            sent += count as u64;
            if sent == manifest.size {
                let mut extra = [0u8; 1];
                if file.read(&mut extra).map_err(|_| "source_read_failed")? != 0
                    || hex(&digest.finalize()) != manifest.sha256
                {
                    return Err("source_changed".into());
                }
                if count > 0 {
                    tx.blocking_send(Ok(Bytes::from(buffer)))
                        .map_err(|_| "upload_stopped")?;
                }
                self.progress(id, sent, false)?;
                return Ok(());
            }
            tx.blocking_send(Ok(Bytes::from(buffer)))
                .map_err(|_| "upload_stopped")?;
            self.progress(id, sent, false)?;
        }
    }

    async fn query_result(&self, id: &str, peer: &PeerInfo) -> Result<(), String> {
        {
            let state = self.lock()?;
            if !state.peers.contains_key(&peer.fingerprint) {
                return Err("untrusted_peer".into());
            }
        }
        let (client, address) = self.peer_client(peer)?;
        let reply = control(
            client.get(url(address, &format!("/v1/transfers/{id}"))),
            &self.stop,
        )
        .await?;
        self.apply_remote(id, reply)?;
        Ok(())
    }

    fn apply_remote(&self, id: &str, reply: TransferReply) -> Result<bool, String> {
        let mut state = self.lock()?;
        let fingerprint = self.task_peer(&state, id)?;
        if !state.peers.contains_key(&fingerprint) {
            return Err("untrusted_peer".into());
        }
        let task = state.tasks.get_mut(id).ok_or("task_not_found")?;
        validate_reply(
            &reply,
            task.manifest.as_ref().ok_or("manifest_unavailable")?,
        )?;
        if terminal(&task.info.status) {
            return Ok(true);
        }
        if terminal(&reply.status) {
            if reply.status == "completed" {
                task.info.bytes_transferred = task.info.size;
            }
            // Never display arbitrary remote text as a local diagnostic.
            task.finish(
                &reply.status,
                if reply.status == "failed" {
                    Some("remote_transfer_failed")
                } else {
                    None
                },
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

enum Piece {
    Data(Bytes),
    End,
}

fn owned_task<'a>(
    state: &'a mut State,
    fingerprint: &str,
    id: &str,
) -> Result<&'a mut Task, String> {
    state
        .tasks
        .get_mut(id)
        .filter(|t| t.info.direction == "receive" && t.info.peer_fingerprint == fingerprint)
        .ok_or_else(|| "task_not_found".into())
}

fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "rejected" | "cancelled" | "failed")
}

fn valid_name(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("invalid_device_name".into());
    }
    let value = value.trim();
    if !(1..=64).contains(&value.chars().count()) {
        return Err("invalid_device_name".into());
    }
    Ok(value.into())
}

pub fn normalize_fingerprint(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let plain = if bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit) {
        value.to_owned()
    } else if bytes.len() == 95
        && bytes.iter().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                *b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        value.replace(':', "")
    } else {
        return Err("full_sha256_fingerprint_required".into());
    };
    Ok(plain
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| String::from_utf8_lossy(b).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":"))
}

fn allowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        IpAddr::V6(_) => false,
    }
}

fn endpoint(address: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: Ipv4Addr = address.parse().map_err(|_| "ipv4_literal_required")?;
    if !allowed_ip(IpAddr::V4(ip)) || port < 1024 {
        return Err("lan_address_and_unprivileged_port_required".into());
    }
    Ok(SocketAddr::new(IpAddr::V4(ip), port))
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "secure_random_unavailable")?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_manifest(m: &Manifest) -> Result<(), String> {
    if m.version != 1 {
        return Err("unsupported_version".into());
    }
    if !valid_id(&m.id) || !valid_id(&m.file_id) {
        return Err("invalid_manifest_id".into());
    }
    if m.size > MAX_FILE {
        return Err("file_too_large".into());
    }
    if !valid_id(&m.sha256) {
        return Err("invalid_sha256".into());
    }
    storage::validate_name(&m.name).map_err(|_| "invalid_file_name".into())
}

fn validate_reply(reply: &TransferReply, m: &Manifest) -> Result<(), String> {
    if reply.version != 1
        || reply.id != m.id
        || reply.file_id != m.file_id
        || reply.name != m.name
        || reply.size != m.size
        || reply.sha256 != m.sha256
    {
        return Err("remote_manifest_mismatch".into());
    }
    if !matches!(
        reply.status.as_str(),
        "awaitingAcceptance"
            | "ready"
            | "transferring"
            | "verifying"
            | "completed"
            | "rejected"
            | "cancelled"
            | "failed"
            | "unknown"
    ) {
        return Err("invalid_remote_status".into());
    }
    Ok(())
}

fn parse_trust(bytes: &[u8]) -> Result<BTreeMap<String, PeerInfo>, String> {
    if bytes.len() > BLOCK {
        return Err("trust_file_too_large".into());
    }
    let doc: TrustDocument =
        serde_json::from_slice(bytes).map_err(|_| "trust_file_damaged_not_overwritten")?;
    if doc.version != 1 || doc.peers.len() > MAX_PEERS {
        return Err("invalid_trust_document".into());
    }
    let mut peers = BTreeMap::new();
    for mut peer in doc.peers {
        peer.fingerprint = normalize_fingerprint(&peer.fingerprint)?;
        peer.name = valid_name(&peer.name)?;
        peer.address = endpoint(&peer.address, peer.port)?.ip().to_string();
        if peers.contains_key(&peer.fingerprint)
            || peers
                .values()
                .any(|p: &PeerInfo| p.address == peer.address && p.port == peer.port)
        {
            return Err("duplicate_trust_entry".into());
        }
        peers.insert(peer.fingerprint.clone(), peer);
    }
    Ok(peers)
}

fn special_file(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

#[allow(deprecated)]
fn constant_equal(a: &[u8], b: &[u8]) -> bool {
    ring::constant_time::verify_slices_are_equal(a, b).is_ok()
}

fn single_header<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Result<&'a str, String> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or("required_header_missing")?
        .to_str()
        .map_err(|_| "invalid_header")?;
    if values.next().is_some() {
        return Err("duplicate_header".into());
    }
    Ok(value)
}

fn no_body(request: &Request<Incoming>) -> Result<(), String> {
    if request.headers().contains_key(header::TRANSFER_ENCODING)
        || request
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v != "0")
    {
        return Err("body_not_allowed".into());
    }
    Ok(())
}

async fn request_json<T: DeserializeOwned>(
    request: Request<Incoming>,
    stop: &CancellationToken,
) -> Result<T, String> {
    if request.headers().contains_key(header::CONTENT_ENCODING) {
        return Err("encoding_not_allowed".into());
    }
    let read = async {
        let mut body = request.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| "invalid_request_body")?;
            let data = frame.into_data().map_err(|_| "trailers_not_allowed")?;
            if bytes.len().saturating_add(data.len()) > MAX_JSON {
                return Err("request_too_large".to_owned());
            }
            bytes.extend_from_slice(&data);
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid_json".into())
    };
    tokio::select! {
        _ = stop.cancelled() => Err("shutting_down".into()),
        result = timeout(CONTROL_TIMEOUT, read) => result.map_err(|_| "request_timeout")?,
    }
}

async fn control<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    stop: &CancellationToken,
) -> Result<T, String> {
    tokio::select! {
        _ = stop.cancelled() => Err("cancelled".into()),
        result = timeout(CONTROL_TIMEOUT, async {
            let response = request.send().await.map_err(|_| "peer_request_failed")?;
            response_json(response, stop).await
        }) => result.map_err(|_| "peer_request_timeout")?,
    }
}

async fn response_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    stop: &CancellationToken,
) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(format!("peer_http_{}", response.status().as_u16()));
    }
    let read = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "peer_body_interrupted")?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_JSON {
                return Err("peer_response_too_large".to_owned());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid_peer_response".into())
    };
    tokio::select! {
        _ = stop.cancelled() => Err("shutting_down".into()),
        result = timeout(CONTROL_TIMEOUT, read) => result.map_err(|_| "peer_response_timeout")?,
    }
}

fn url(address: SocketAddr, path: &str) -> String {
    format!("https://localhost:{}{path}", address.port())
}

async fn pause(stop: &CancellationToken) -> Result<(), String> {
    tokio::select! {
        _ = stop.cancelled() => Err("cancelled".into()),
        _ = tokio::time::sleep(Duration::from_millis(400)) => Ok(()),
    }
}

fn json_response(value: &impl Serialize) -> Result<Response<Full<Bytes>>, String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "response_encode_failed")?;
    let mut response = Response::new(Full::new(Bytes::from(bytes)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(
        header::CONNECTION,
        header::HeaderValue::from_static("close"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

fn failure(code: &str) -> Response<Full<Bytes>> {
    let status = match code {
        "untrusted_peer" | "invalid_identity" | "invalid_grant" => StatusCode::FORBIDDEN,
        "task_not_found" | "not_found" | "pair_not_found_or_expired" => StatusCode::NOT_FOUND,
        "pair_rate_limited" | "pair_requests_full" | "history_full" | "direction_busy" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "grant_unavailable" | "manifest_conflict" | "endpoint_identity_changed_revoke_first" => {
            StatusCode::CONFLICT
        }
        "shutting_down" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(96)
        .collect();
    let mut response = json_response(&serde_json::json!({"error": code}))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
