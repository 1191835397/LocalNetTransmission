use super::*;
use crate::{identity, service, tls};

struct Node {
    config: tempfile::TempDir,
    identity: Arc<Identity>,
    engine: Arc<Engine>,
    service: service::ServiceHandle,
}

impl Node {
    async fn new(name: &str) -> Self {
        let config = tempfile::tempdir().unwrap();
        let identity = Arc::new(identity::generate().unwrap());
        let directory = config.path().to_owned();
        let id = identity.clone();
        let name = name.to_owned();
        let engine = tokio::task::spawn_blocking(move || Engine::new(directory, id, name))
            .await
            .unwrap()
            .unwrap();
        let service = service::start_on(&identity, Ipv4Addr::LOCALHOST, 0, engine.clone()).await;
        assert_eq!(service.snapshot().state, service::ServiceState::Running);
        Self {
            config,
            identity,
            engine,
            service,
        }
    }

    fn address(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.service.snapshot().port))
    }

    fn client_to(&self, other: &Node) -> reqwest::Client {
        tls::client(&self.identity, &other.identity.fingerprint, other.address()).unwrap()
    }

    async fn close(self) {
        self.engine.shutdown().await;
        self.service.shutdown().await;
        drop(self.engine);
        drop(self.config);
    }
}

async fn wait_for(
    engine: &Engine,
    predicate: impl Fn(&TransferSnapshot) -> bool,
) -> TransferSnapshot {
    timeout(Duration::from_secs(180), async {
        loop {
            let snapshot = engine.snapshot().unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("engine did not reach expected state")
}

async fn pair(sender: &Node, receiver: &Node) {
    let engine = sender.engine.clone();
    let port = receiver.address().port();
    let fingerprint = receiver.identity.fingerprint.clone();
    let connection =
        tokio::spawn(async move { engine.connect("127.0.0.1".into(), port, fingerprint).await });
    let pending = wait_for(&receiver.engine, |s| !s.pair_requests.is_empty()).await;
    assert_eq!(
        pending.pair_requests[0].fingerprint,
        sender.identity.fingerprint
    );
    let engine = receiver.engine.clone();
    let id = pending.pair_requests[0].id.clone();
    tokio::task::spawn_blocking(move || engine.respond_pair(id, true))
        .await
        .unwrap()
        .unwrap();
    let peer = connection.await.unwrap().unwrap();
    assert_eq!(peer.fingerprint, receiver.identity.fingerprint);
    assert_eq!(sender.engine.snapshot().unwrap().peers.len(), 1);
}

fn manifest(bytes: &[u8], name: &str) -> Manifest {
    Manifest {
        version: 1,
        id: random_id().unwrap(),
        file_id: random_id().unwrap(),
        name: name.into(),
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
    }
}

async fn offer(client: &reqwest::Client, receiver: &Node, manifest: &Manifest) -> TransferReply {
    control(
        client
            .post(url(receiver.address(), "/v1/offers"))
            .json(manifest),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn content(
    client: &reqwest::Client,
    receiver: &Node,
    manifest: &Manifest,
    token: &str,
) -> reqwest::RequestBuilder {
    client
        .put(url(
            receiver.address(),
            &format!("/v1/transfers/{}/content", manifest.id),
        ))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("x-file-id", &manifest.file_id)
        .header("x-offset", "0")
        .header(header::CONTENT_LENGTH, manifest.size)
}

fn accept(receiver: &Node, manifest: &Manifest, directory: &Path) -> String {
    receiver
        .engine
        .respond_transfer(
            manifest.id.clone(),
            true,
            Some(directory.to_string_lossy().into_owned()),
        )
        .unwrap();
    let state = receiver.engine.lock().unwrap();
    state
        .tasks
        .get(&manifest.id)
        .unwrap()
        .token
        .clone()
        .unwrap()
}

#[test]
fn strict_full_fingerprints_ipv4_and_device_names() {
    let fingerprint = identity::fingerprint(b"test");
    assert_eq!(
        normalize_fingerprint(&fingerprint.to_lowercase()).unwrap(),
        fingerprint
    );
    assert_eq!(
        normalize_fingerprint(&fingerprint.replace(':', "")).unwrap(),
        fingerprint
    );
    for invalid in [
        "".into(),
        "ab".repeat(31),
        "gg".repeat(32),
        format!(" {fingerprint}"),
        "aa:".repeat(32),
        "a".repeat(95),
    ] {
        assert!(normalize_fingerprint(&invalid).is_err());
    }
    for address in [
        "8.8.8.8",
        "224.0.0.1",
        "255.255.255.255",
        "0.0.0.0",
        "100.64.0.1",
        "::1",
        "localhost",
        "127.1",
        "https://127.0.0.1",
    ] {
        assert!(endpoint(address, 53318).is_err(), "{address}");
    }
    for address in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.1.1",
    ] {
        assert!(endpoint(address, 1024).is_ok());
        assert!(endpoint(address, 65535).is_ok());
        assert!(endpoint(address, 1023).is_err());
    }
    for name in ["", " ", "a\nb", "a\0b", "\tx", &"中".repeat(65)] {
        assert!(valid_name(name).is_err());
    }
    assert_eq!(valid_name("  中文  ").unwrap(), "中文");
    assert!(valid_name(&"中".repeat(64)).is_ok());
}

#[test]
fn malformed_manifests_and_untrusted_documents_are_rejected() {
    let original = manifest(b"payload", "valid.txt");
    for name in [
        "../outside",
        "a/b",
        "a\\b",
        "C:\\escape",
        "/escape",
        "a:stream",
        "CON",
        "..",
        "",
    ] {
        let mut m = original.clone();
        m.name = name.into();
        assert!(validate_manifest(&m).is_err(), "{name}");
    }
    let mut m = original.clone();
    m.version = 2;
    assert!(validate_manifest(&m).is_err());
    m = original.clone();
    m.size = MAX_FILE + 1;
    assert!(validate_manifest(&m).is_err());
    m = original.clone();
    m.sha256 = "00".into();
    assert!(validate_manifest(&m).is_err());
    for data in [
        b"broken".as_slice(),
        br#"{"version":2,"peers":[]}"#,
        br#"{"version":1,"peers":[],"extra":1}"#,
    ] {
        assert!(parse_trust(data).is_err());
    }
    assert!(parse_trust(&vec![b' '; BLOCK + 1]).is_err());
    let peer = PeerInfo {
        fingerprint: identity::fingerprint(b"peer"),
        name: "peer".into(),
        address: "127.0.0.1".into(),
        port: 1234,
    };
    let duplicate = TrustDocument {
        version: 1,
        peers: vec![peer.clone(), peer.clone()],
    };
    assert!(parse_trust(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    let oversized = TrustDocument {
        version: 1,
        peers: vec![peer; MAX_PEERS + 1],
    };
    assert!(parse_trust(&serde_json::to_vec(&oversized).unwrap()).is_err());
}

#[test]
fn trust_lock_corruption_and_external_edits_never_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let (first, peers) = TrustStore::open(directory.path().to_owned()).unwrap();
    assert!(TrustStore::open(directory.path().to_owned()).is_err());
    first.persist(&peers).unwrap();
    let path = directory.path().join("peers.json");
    std::fs::write(&path, b"damaged").unwrap();
    assert!(first.persist(&peers).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
    drop(first);
    assert!(TrustStore::open(directory.path().to_owned()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
}

#[test]
fn terminal_statuses_never_regress_and_wire_excludes_local_paths() {
    let peer = PeerInfo {
        fingerprint: identity::fingerprint(b"peer"),
        name: "peer".into(),
        address: "127.0.0.1".into(),
        port: 1234,
    };
    let m = manifest(b"", "empty");
    let mut task = Task::new(m.id.clone(), &peer, "receive", m.name.clone(), 0);
    task.manifest = Some(m);
    task.directory = Some(PathBuf::from("private-directory"));
    task.info.saved_path = Some("private-directory/empty".into());
    task.finish("completed", None);
    task.finish("cancelled", Some("cancelled"));
    task.finish("failed", Some("failed"));
    assert_eq!(task.info.status, "completed");
    let wire = serde_json::to_string(&task.reply().unwrap()).unwrap();
    assert!(
        !wire.contains("savedPath")
            && !wire.contains("private-directory")
            && !wire.contains("directory")
    );
    let ipc = serde_json::to_value(&task.info).unwrap();
    for key in [
        "peerFingerprint",
        "peerName",
        "fileName",
        "bytesTransferred",
        "bytesPerSecond",
        "savedPath",
    ] {
        assert!(ipc.get(key).is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_pairing_rejection_owner_status_and_identity_pin() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    let stranger = Node::new("stranger").await;
    let error = sender
        .engine
        .connect(
            "127.0.0.1".into(),
            receiver.address().port(),
            stranger.identity.fingerprint.clone(),
        )
        .await;
    assert!(error.is_err());
    assert!(receiver.engine.snapshot().unwrap().pair_requests.is_empty());
    let engine = sender.engine.clone();
    let port = receiver.address().port();
    let fp = receiver.identity.fingerprint.clone();
    let connection =
        tokio::spawn(async move { engine.connect("127.0.0.1".into(), port, fp).await });
    let pending = wait_for(&receiver.engine, |s| !s.pair_requests.is_empty()).await;
    let id = pending.pair_requests[0].id.clone();
    let response = stranger
        .client_to(&receiver)
        .get(url(receiver.address(), &format!("/v1/pair/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(sender.engine.snapshot().unwrap().peers.is_empty());
    assert!(receiver.engine.snapshot().unwrap().peers.is_empty());
    let engine = receiver.engine.clone();
    tokio::task::spawn_blocking(move || engine.respond_pair(id, false))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(connection.await.unwrap().unwrap_err(), "pair_rejected");
    assert!(!receiver.config.path().join("peers.json").exists());
    assert!(!sender.config.path().join("peers.json").exists());
    let response = sender
        .client_to(&receiver)
        .post(url(receiver.address(), "/v1/offers"))
        .json(&manifest(b"x", "x"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    sender.close().await;
    receiver.close().await;
    stranger.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_manifest_binding_rejection_bad_hash_token_reuse_and_result_lookup() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    let other = Node::new("other").await;
    pair(&sender, &receiver).await;
    pair(&other, &receiver).await;
    let directory = tempfile::tempdir().unwrap();
    let client = sender.client_to(&receiver);
    let m = manifest(b"content", "payload.bin");
    assert_eq!(
        offer(&client, &receiver, &m).await.status,
        "awaitingAcceptance"
    );
    assert_eq!(
        offer(&client, &receiver, &m).await.status,
        "awaitingAcceptance"
    );
    assert_eq!(receiver.engine.snapshot().unwrap().tasks.len(), 1);
    let mut changed = m.clone();
    changed.name = "changed".into();
    assert_eq!(
        client
            .post(url(receiver.address(), "/v1/offers"))
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let token = accept(&receiver, &m, directory.path());
    let private_status = other
        .client_to(&receiver)
        .get(url(receiver.address(), &format!("/v1/transfers/{}", m.id)))
        .send()
        .await
        .unwrap();
    assert_eq!(private_status.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        content(&other.client_to(&receiver), &receiver, &m, &token)
            .body("content")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        content(&client, &receiver, &m, &random_id().unwrap())
            .body("content")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut wrong_length = m.clone();
    wrong_length.size += 1;
    assert_eq!(
        content(&client, &receiver, &wrong_length, &token)
            .body("contents")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        content(&client, &receiver, &m, &token)
            .header("content-encoding", "gzip")
            .body("content")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let wrong_file_id = random_id().unwrap();
    assert_ne!(wrong_file_id, m.file_id);
    for (name, value) in [("x-file-id", wrong_file_id), ("x-offset", "1".into())] {
        let mut request = content(&client, &receiver, &m, &token)
            .body("content")
            .build()
            .unwrap();
        // Replace, rather than append, so this exercises binding, not duplicate headers.
        request.headers_mut().insert(name, value.parse().unwrap());
        let response = client.execute(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            "file_binding_mismatch",
            "{name}"
        );
        let reply = offer(&client, &receiver, &m).await;
        assert_eq!(reply.status, "ready");
        assert_eq!(reply.token.as_deref(), Some(token.as_str()));
        {
            let state = receiver.engine.lock().unwrap();
            let task = state.tasks.get(&m.id).unwrap();
            assert!(!task.claimed);
            assert!(!task.running);
            assert_eq!(task.info.bytes_transferred, 0);
            assert!(task.info.saved_path.is_none());
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    assert_eq!(
        content(&client, &receiver, &m, &token)
            .body("corrupt")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let reply = offer(&client, &receiver, &m).await;
    assert_eq!(reply.status, "failed");
    // The same token reached content verification after the invalid binding attempts.
    assert_eq!(reply.error.as_deref(), Some("digest_mismatch"));
    assert!(reply.token.is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert_eq!(
        content(&client, &receiver, &m, &token)
            .body("content")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let rejected = manifest(b"no", "rejected.bin");
    offer(&client, &receiver, &rejected).await;
    receiver
        .engine
        .respond_transfer(rejected.id.clone(), false, None)
        .unwrap();
    assert_eq!(
        offer(&client, &receiver, &rejected).await.status,
        "rejected"
    );
    let cancelled = manifest(b"no", "cancelled.bin");
    offer(&client, &receiver, &cancelled).await;
    let token = accept(&receiver, &cancelled, directory.path());
    receiver.engine.cancel(cancelled.id.clone()).unwrap();
    assert_eq!(
        content(&client, &receiver, &cancelled, &token)
            .body("no")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let m = manifest(b"saved", "saved.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, directory.path());
    // Deliberately discard the final response body: subsequent authenticated status
    // lookup, not an assumed successful upload, supplies the durable result.
    let response = content(&client, &receiver, &m, &token)
        .body("saved")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    let reply = offer(&client, &receiver, &m).await;
    assert_eq!(reply.status, "completed");
    assert!(reply.token.is_none());
    assert_eq!(
        std::fs::read(directory.path().join("saved.bin")).unwrap(),
        b"saved"
    );
    assert_eq!(
        content(&client, &receiver, &m, &token)
            .body("saved")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    // Refresh the sender's retained unknown record against the real pinned TLS service.
    {
        let receiver_peer = sender.engine.snapshot().unwrap().peers[0].clone();
        let mut task = Task::new(m.id.clone(), &receiver_peer, "send", m.name.clone(), m.size);
        task.manifest = Some(m.clone());
        task.upload_started = true;
        task.finish("unknown", Some("ack_discarded"));
        sender
            .engine
            .lock()
            .unwrap()
            .tasks
            .insert(m.id.clone(), task);
    }
    sender.engine.refresh_result(m.id.clone()).await.unwrap();
    assert_eq!(
        sender.engine.snapshot().unwrap().tasks[0].status,
        "completed"
    );
    sender.close().await;
    receiver.close().await;
    other.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_expired_upload_token_cannot_create_output() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().canonicalize().unwrap();
    let client = sender.client_to(&receiver);
    let m = manifest(b"expired", "expired.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, &destination);
    {
        let mut state = receiver.engine.lock().unwrap();
        let task = state.tasks.get_mut(&m.id).unwrap();
        assert_eq!(task.info.status, "ready");
        assert_eq!(task.token.as_deref(), Some(token.as_str()));
        assert!(!task.claimed);
        // Let the real PUT encounter the expired grant without a preceding status request.
        task.deadline = Some(Instant::now() - Duration::from_secs(1));
    }
    let response = content(&client, &receiver, &m, &token)
        .body("expired")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "grant_unavailable"
    );
    let reply = offer(&client, &receiver, &m).await;
    assert_eq!(reply.status, "failed");
    assert_eq!(reply.error.as_deref(), Some("acceptance_expired"));
    assert!(reply.token.is_none());
    assert_eq!(
        receiver.engine.respond_transfer(
            m.id.clone(),
            true,
            Some(destination.to_string_lossy().into_owned())
        ),
        Err("transfer_already_decided".into())
    );
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    timeout(Duration::from_secs(5), receiver.engine.shutdown())
        .await
        .unwrap();
    {
        let state = receiver.engine.lock().unwrap();
        let task = state.tasks.get(&m.id).unwrap();
        assert_eq!(task.info.status, "failed");
        assert!(!task.claimed);
        assert!(!task.running);
        assert!(task.token.is_none());
        assert_eq!(task.info.bytes_transferred, 0);
        assert!(task.info.saved_path.is_none());
    }
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    sender.close().await;
    receiver.close().await;
}

async fn transfer_size(sender: &Node, receiver: &Node, size: u64) {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = source.path().join(format!("payload-{size}.bin"));
    let write_path = path.clone();
    let expected = tokio::task::spawn_blocking(move || {
        let mut file = File::create(write_path).unwrap();
        let buffer: Vec<u8> = (0..BLOCK).map(|i| ((i * 17 + 13) % 251) as u8).collect();
        let mut digest = Sha256::new();
        let mut remaining = size;
        while remaining != 0 {
            let count = remaining.min(BLOCK as u64) as usize;
            file.write_all(&buffer[..count]).unwrap();
            digest.update(&buffer[..count]);
            remaining -= count as u64;
        }
        hex(&digest.finalize())
    })
    .await
    .unwrap();
    let id = sender
        .engine
        .send(
            receiver.identity.fingerprint.clone(),
            path.to_string_lossy().into_owned(),
        )
        .unwrap();
    let pending = wait_for(&receiver.engine, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == id && t.status == "awaitingAcceptance")
    })
    .await;
    assert_eq!(
        pending.tasks.iter().find(|t| t.id == id).unwrap().size,
        size
    );
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    receiver
        .engine
        .respond_transfer(
            id.clone(),
            true,
            Some(destination.path().to_string_lossy().into_owned()),
        )
        .unwrap();
    let sent = wait_for(&sender.engine, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == id && (terminal(&t.status) || t.status == "unknown"))
    })
    .await;
    let sent = sent.tasks.iter().find(|t| t.id == id).unwrap();
    let received = receiver.engine.snapshot().unwrap();
    let received = received.tasks.iter().find(|t| t.id == id).unwrap();
    assert_eq!(
        sent.status, "completed",
        "send error: {:?}; receive error: {:?}; received bytes: {}",
        sent.error, received.error, received.bytes_transferred
    );
    assert_eq!(received.status, "completed");
    assert_eq!(received.bytes_transferred, size);
    let saved = PathBuf::from(received.saved_path.clone().unwrap());
    assert!(saved.starts_with(destination.path()));
    let actual = tokio::task::spawn_blocking(move || {
        let mut file = File::open(saved).unwrap();
        let mut digest = Sha256::new();
        let mut buffer = vec![0; BLOCK];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        hex(&digest.finalize())
    })
    .await
    .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_engine_empty_1k_and_100m_both_directions() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    transfer_size(&sender, &receiver, 0).await;
    transfer_size(&receiver, &sender, 1024).await;
    transfer_size(&sender, &receiver, 100 * 1024 * 1024).await;
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "1 GiB real TLS/disk test; run explicitly with --ignored"]
async fn real_tls_engine_one_gibibyte() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    transfer_size(&sender, &receiver, 1024 * 1024 * 1024).await;
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_revocation_and_shutdown_join_streaming_writers() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    let directory = tempfile::tempdir().unwrap();
    let client = sender.client_to(&receiver);
    let m = manifest(&vec![7; BLOCK * 4], "partial.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, directory.path());
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let stream = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|piece| (piece, rx))
    });
    let request = content(&client, &receiver, &m, &token).body(reqwest::Body::wrap_stream(stream));
    let upload = tokio::spawn(async move { request.send().await });
    tx.send(Ok(Bytes::from(vec![7; BLOCK]))).await.unwrap();
    wait_for(&receiver.engine, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == m.id && t.bytes_transferred > 0)
    })
    .await;
    let engine = receiver.engine.clone();
    let fp = sender.identity.fingerprint.clone();
    tokio::task::spawn_blocking(move || engine.forget_peer(fp))
        .await
        .unwrap()
        .unwrap();
    drop(tx);
    let _ = upload.await.unwrap();
    timeout(Duration::from_secs(5), receiver.engine.shutdown())
        .await
        .unwrap();
    let task = receiver
        .engine
        .snapshot()
        .unwrap()
        .tasks
        .into_iter()
        .find(|t| t.id == m.id)
        .unwrap();
    assert_eq!(task.status, "cancelled");
    assert!(task.saved_path.is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let trust = TrustStore::read(receiver.config.path()).unwrap().unwrap();
    assert!(parse_trust(&trust).unwrap().is_empty());
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_sender_rejection_cancel_and_trusted_reconnect() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    sender
        .engine
        .connect(
            "127.0.0.1".into(),
            receiver.address().port(),
            receiver.identity.fingerprint.clone(),
        )
        .await
        .unwrap();
    assert!(receiver.engine.snapshot().unwrap().pair_requests.is_empty());
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("source.bin");
    std::fs::write(&path, b"test contents").unwrap();
    let id = sender
        .engine
        .send(
            receiver.identity.fingerprint.clone(),
            path.to_string_lossy().into_owned(),
        )
        .unwrap();
    assert!(sender
        .engine
        .send(
            receiver.identity.fingerprint.clone(),
            path.to_string_lossy().into_owned()
        )
        .is_err());
    wait_for(&receiver.engine, |s| s.tasks.iter().any(|t| t.id == id)).await;
    receiver
        .engine
        .respond_transfer(id.clone(), false, None)
        .unwrap();
    let snapshot = wait_for(&sender.engine, |s| {
        s.tasks.iter().any(|t| t.id == id && terminal(&t.status))
    })
    .await;
    assert_eq!(
        snapshot.tasks.iter().find(|t| t.id == id).unwrap().status,
        "rejected"
    );
    // The completed worker must release its direction reservation before another send.
    timeout(Duration::from_secs(5), async {
        loop {
            if !sender
                .engine
                .lock()
                .unwrap()
                .tasks
                .get(&id)
                .unwrap()
                .running
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let id = sender
        .engine
        .send(
            receiver.identity.fingerprint.clone(),
            path.to_string_lossy().into_owned(),
        )
        .unwrap();
    wait_for(&receiver.engine, |s| s.tasks.iter().any(|t| t.id == id)).await;
    sender.engine.cancel(id.clone()).unwrap();
    let snapshot = wait_for(&receiver.engine, |s| {
        s.tasks.iter().any(|t| t.id == id && terminal(&t.status))
    })
    .await;
    assert_eq!(
        snapshot.tasks.iter().find(|t| t.id == id).unwrap().status,
        "cancelled"
    );
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_progressing_upload_outlasts_control_timeouts() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    let directory = tempfile::tempdir().unwrap();
    let client = sender.client_to(&receiver);
    let expected = vec![7; BLOCK * 17];
    let m = manifest(&expected, "slow.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, directory.path());
    let body = stream::unfold(0, |index| async move {
        if index == 17 {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        Some((
            Ok::<_, std::io::Error>(Bytes::from(vec![7; BLOCK])),
            index + 1,
        ))
    });
    let started = Instant::now();
    let response = timeout(
        Duration::from_secs(50),
        content(&client, &receiver, &m, &token)
            .body(reqwest::Body::wrap_stream(body))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() > IDLE);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<TransferReply>().await.unwrap().status,
        "completed"
    );
    assert_eq!(
        std::fs::read(directory.path().join("slow.bin")).unwrap(),
        expected
    );
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_incomplete_stream_is_failed_and_clean_after_shutdown() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().canonicalize().unwrap();
    let client = sender.client_to(&receiver);
    let m = manifest(&vec![5; BLOCK * 2], "truncated.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, &destination);
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let body = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    // The declared length matches the manifest; only the actual stream is short.
    let request = content(&client, &receiver, &m, &token).body(reqwest::Body::wrap_stream(body));
    let upload = tokio::spawn(async move { request.send().await });
    tx.send(Ok(Bytes::from(vec![5; BLOCK]))).await.unwrap();
    let receiving = wait_for(&receiver.engine, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == m.id && t.bytes_transferred == BLOCK as u64)
    })
    .await;
    let task = receiving.tasks.iter().find(|t| t.id == m.id).unwrap();
    assert_eq!(task.status, "transferring");
    assert!(task.saved_path.is_none());
    // Observe the staging entry without opening the exclusively locked payload.
    let entries = std::fs::read_dir(&destination)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].file_name().to_string_lossy().ends_with(".part"));
    drop(entries);
    assert!(!destination.join(&m.name).exists());
    drop(tx);
    // HTTP/1 may lose the error response when the short request body closes.
    if let Ok(response) = timeout(Duration::from_secs(10), upload)
        .await
        .expect("short upload did not finish")
        .unwrap()
    {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let failed = wait_for(&receiver.engine, |s| {
        s.tasks.iter().any(|t| t.id == m.id && terminal(&t.status))
    })
    .await;
    let task = failed.tasks.iter().find(|t| t.id == m.id).unwrap();
    assert_eq!(task.status, "failed");
    assert_eq!(task.error.as_deref(), Some("body_interrupted"));
    assert_eq!(task.bytes_transferred, BLOCK as u64);
    assert!(task.saved_path.is_none());
    timeout(Duration::from_secs(5), receiver.engine.shutdown())
        .await
        .unwrap();
    {
        let state = receiver.engine.lock().unwrap();
        let task = state.tasks.get(&m.id).unwrap();
        assert_eq!(task.info.status, "failed");
        assert_eq!(task.info.error.as_deref(), Some("body_interrupted"));
        assert!(task.claimed);
        assert!(task.token.is_none());
        assert!(!task.running);
        assert!(task.info.saved_path.is_none());
    }
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    sender.close().await;
    receiver.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tls_shutdown_cancels_idle_body_without_detaching_writer() {
    let sender = Node::new("sender").await;
    let receiver = Node::new("receiver").await;
    pair(&sender, &receiver).await;
    let destination = tempfile::tempdir().unwrap();
    let client = sender.client_to(&receiver);
    let m = manifest(&vec![3; BLOCK * 2], "incomplete.bin");
    offer(&client, &receiver, &m).await;
    let token = accept(&receiver, &m, destination.path());
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let body = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    let request = content(&client, &receiver, &m, &token).body(reqwest::Body::wrap_stream(body));
    let upload = tokio::spawn(async move { request.send().await });
    tx.send(Ok(Bytes::from(vec![3; BLOCK]))).await.unwrap();
    let receiving = wait_for(&receiver.engine, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == m.id && t.bytes_transferred == BLOCK as u64)
    })
    .await;
    let task = receiving.tasks.iter().find(|t| t.id == m.id).unwrap();
    assert_eq!(task.status, "transferring");
    assert!(task.saved_path.is_none());
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    // Keep the sender channel open. Shutdown must interrupt the idle body itself.
    timeout(Duration::from_secs(5), receiver.engine.shutdown())
        .await
        .unwrap();
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    {
        let state = receiver.engine.lock().unwrap();
        let task = state.tasks.get(&m.id).unwrap();
        assert_eq!(task.info.status, "cancelled");
        assert_eq!(task.info.error.as_deref(), Some("shutting_down"));
        assert_eq!(task.info.bytes_transferred, BLOCK as u64);
        assert!(task.info.saved_path.is_none());
        assert!(task.claimed);
        assert!(task.token.is_none());
        assert!(!task.running);
    }
    drop(tx);
    let _ = upload.await.unwrap();
    sender.close().await;
    receiver.close().await;
}

#[tokio::test]
async fn pending_caps_expiry_and_history_retention_are_bounded() {
    let node = Node::new("receiver").await;
    for i in 0..8u16 {
        let fp = identity::fingerprint(&i.to_be_bytes());
        node.engine
            .receive_pair(
                PairHello {
                    version: 1,
                    name: format!("peer-{i}"),
                    port: 1024 + i,
                },
                &fp,
                SocketAddr::from((Ipv4Addr::LOCALHOST, 50000)),
            )
            .unwrap();
    }
    assert!(node
        .engine
        .receive_pair(
            PairHello {
                version: 1,
                name: "ninth".into(),
                port: 1033
            },
            &identity::fingerprint(b"ninth"),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 50000))
        )
        .is_err());
    assert_eq!(node.engine.snapshot().unwrap().pair_requests.len(), 8);
    {
        let mut state = node.engine.lock().unwrap();
        for pair in state.pairs.values_mut() {
            pair.deadline = Instant::now() - Duration::from_secs(1);
        }
    }
    assert!(node.engine.snapshot().unwrap().pair_requests.is_empty());
    let peer = PeerInfo {
        fingerprint: identity::fingerprint(b"peer"),
        name: "peer".into(),
        address: "127.0.0.1".into(),
        port: 1234,
    };
    {
        let mut state = node.engine.lock().unwrap();
        state.peers.insert(peer.fingerprint.clone(), peer.clone());
        for _ in 0..MAX_TASKS {
            let m = manifest(b"", "empty");
            let mut task = Task::new(m.id.clone(), &peer, "receive", "empty".into(), 0);
            task.manifest = Some(m.clone());
            task.finish("rejected", None);
            state.tasks.insert(m.id, task);
        }
    }
    assert_eq!(node.engine.snapshot().unwrap().tasks.len(), MAX_TASKS);
    assert!(node
        .engine
        .offer(&peer.fingerprint, manifest(b"", "empty"))
        .is_err());
    let existing = node
        .engine
        .lock()
        .unwrap()
        .tasks
        .values()
        .next()
        .unwrap()
        .manifest
        .clone()
        .unwrap();
    assert_eq!(
        node.engine
            .offer(&peer.fingerprint, existing)
            .unwrap()
            .status,
        "rejected"
    );
    node.close().await;
}
