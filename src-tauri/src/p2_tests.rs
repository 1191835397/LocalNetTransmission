use crate::{engine::Engine, identity, service, tls};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout};

struct Device {
    _directory: tempfile::TempDir,
    identity: Arc<identity::Identity>,
    engine: Arc<Engine>,
    service: service::ServiceHandle,
}

impl Device {
    async fn start(name: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let identity = Arc::new(identity::generate().unwrap());
        let engine =
            Engine::new(directory.path().to_owned(), identity.clone(), name.into()).unwrap();
        let service = service::start_on(&identity, Ipv4Addr::LOCALHOST, 0, engine.clone()).await;
        assert_eq!(service.snapshot().state, service::ServiceState::Running);
        Self {
            _directory: directory,
            identity,
            engine,
            service,
        }
    }

    fn port(&self) -> u16 {
        self.service.snapshot().port
    }

    async fn stop(self) {
        self.engine.shutdown().await;
        self.service.shutdown().await;
    }
}

fn snapshot(engine: &Engine) -> Value {
    serde_json::to_value(engine.snapshot().unwrap()).unwrap()
}

async fn wait_snapshot(engine: &Engine, predicate: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(180), async {
        loop {
            let value = snapshot(engine);
            if predicate(&value) {
                return value;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等待状态超时：{}", snapshot(engine)))
}

async fn pair(sender: &Device, receiver: &Device) {
    let engine = sender.engine.clone();
    let fingerprint = receiver.identity.fingerprint.clone();
    let port = receiver.port();
    let pending =
        tokio::spawn(async move { engine.connect("127.0.0.1".into(), port, fingerprint).await });
    let value = wait_snapshot(&receiver.engine, |value| {
        !value["pairRequests"].as_array().unwrap().is_empty()
    })
    .await;
    let request = &value["pairRequests"][0];
    assert_eq!(request["fingerprint"], sender.identity.fingerprint);
    receiver
        .engine
        .respond_pair(request["id"].as_str().unwrap().into(), true)
        .unwrap();
    let peer = pending.await.unwrap().unwrap();
    assert_eq!(peer.fingerprint, receiver.identity.fingerprint);
    assert_eq!(
        snapshot(&sender.engine)["peers"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        snapshot(&receiver.engine)["peers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

fn task(value: &Value, id: &str) -> Option<Value> {
    value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == id)
        .cloned()
}

fn terminal(task: &Value) -> bool {
    matches!(
        task["status"].as_str(),
        Some("completed" | "rejected" | "cancelled" | "failed" | "unknown")
    )
}

fn write_sample(path: &Path, size: u64) {
    let mut file = fs::File::create(path).unwrap();
    let block = (0..65536)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let mut remaining = size;
    while remaining > 0 {
        let count = remaining.min(block.len() as u64) as usize;
        file.write_all(&block[..count]).unwrap();
        remaining -= count as u64;
    }
    file.sync_all().unwrap();
}

fn digest(path: &Path) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    hash.finalize().to_vec()
}

async fn transfer(sender: &Device, receiver: &Device, source: &Path, destination: &Path) -> Value {
    let id = sender
        .engine
        .send(
            receiver.identity.fingerprint.clone(),
            source.to_str().unwrap().into(),
        )
        .unwrap();
    let offer = wait_snapshot(&receiver.engine, |value| task(value, &id).is_some()).await;
    assert_eq!(task(&offer, &id).unwrap()["status"], "awaitingAcceptance");
    receiver
        .engine
        .respond_transfer(id.clone(), true, Some(destination.to_str().unwrap().into()))
        .unwrap();
    let result = wait_snapshot(&sender.engine, |value| {
        task(value, &id).is_some_and(|task| terminal(&task))
    })
    .await;
    let sent = task(&result, &id).unwrap();
    assert_eq!(sent["status"], "completed", "{sent}");
    let received = task(&snapshot(&receiver.engine), &id).unwrap();
    assert_eq!(received["status"], "completed", "{received}");
    let saved = Path::new(received["savedPath"].as_str().unwrap());
    assert_eq!(digest(source), digest(saved));
    assert_eq!(
        fs::metadata(saved).unwrap().len(),
        fs::metadata(source).unwrap().len()
    );
    sender.engine.refresh_result(id).await.unwrap();
    received
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_identities_transfer_both_directions_and_preserve_same_name() {
    let first = Device::start("Windows A").await;
    let second = Device::start("独立身份 B").await;
    pair(&first, &second).await;
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    let source_root = source_dir.path().canonicalize().unwrap();
    let destination = destination_dir.path().canonicalize().unwrap();
    for size in [0, 1024, 100 * 1024 * 1024] {
        let source = source_root.join(format!("中文 sample {size}.bin"));
        write_sample(&source, size);
        let received = transfer(&first, &second, &source, &destination).await;
        let returned = transfer(
            &second,
            &first,
            Path::new(received["savedPath"].as_str().unwrap()),
            &source_root,
        )
        .await;
        assert_ne!(Path::new(returned["savedPath"].as_str().unwrap()), source);
        assert_eq!(fs::metadata(&source).unwrap().len(), size);
    }
    first.stop().await;
    second.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Explicit 1 GiB disk and network validation"]
async fn one_gib_streaming_integrity() {
    let first = Device::start("Large sender").await;
    let second = Device::start("Large receiver").await;
    pair(&first, &second).await;
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().canonicalize().unwrap().join("1 GiB.bin");
    write_sample(&source, 1024 * 1024 * 1024);
    transfer(
        &first,
        &second,
        &source,
        &destination_dir.path().canonicalize().unwrap(),
    )
    .await;
    first.stop().await;
    second.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejection_and_cancel_do_not_write_files() {
    let source_dir = tempfile::tempdir().unwrap();
    let source_root = source_dir.path().canonicalize().unwrap();
    let source = source_root.join("reject.bin");
    write_sample(&source, 1024);
    for expected in ["rejected", "cancelled"] {
        // Independent engines avoid racing the previous sender's direction reservation.
        let first = Device::start("A").await;
        let second = Device::start("B").await;
        pair(&first, &second).await;
        let id = first
            .engine
            .send(
                second.identity.fingerprint.clone(),
                source.to_str().unwrap().into(),
            )
            .unwrap();
        let pending = wait_snapshot(&second.engine, |value| task(value, &id).is_some()).await;
        assert_eq!(task(&pending, &id).unwrap()["status"], "awaitingAcceptance");
        if expected == "rejected" {
            second
                .engine
                .respond_transfer(id.clone(), false, None)
                .unwrap();
        } else {
            second.engine.cancel(id.clone()).unwrap();
        }
        let result = wait_snapshot(&first.engine, |value| {
            task(value, &id).is_some_and(|task| terminal(&task))
        })
        .await;
        assert_eq!(task(&result, &id).unwrap()["status"], expected);
        // No directory was approved: inspect the real receiver task, not an unrelated folder.
        let received = task(&snapshot(&second.engine), &id).unwrap();
        assert_eq!(received["direction"], "receive");
        assert_eq!(received["status"], expected);
        assert_eq!(received["bytesTransferred"], 0);
        assert_eq!(received.get("savedPath"), Some(&Value::Null));
        assert_eq!(
            second.engine.respond_transfer(
                id.clone(),
                true,
                Some(source_root.to_str().unwrap().into())
            ),
            Err("transfer_already_decided".into())
        );
        assert_eq!(task(&snapshot(&second.engine), &id).unwrap(), received);
        first.stop().await;
        second.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_or_unpaired_peer_cannot_offer() {
    let first = Device::start("A").await;
    let second = Device::start("B").await;
    let client = tls::client(
        &first.identity,
        &second.identity.fingerprint,
        SocketAddr::from((Ipv4Addr::LOCALHOST, second.port())),
    )
    .unwrap();
    let url = format!("https://localhost:{}/v1/offers", second.port());
    let offer = json!({"version":1,"id":"a".repeat(64),"fileId":"b".repeat(64),"name":"sample.bin","size":0,"sha256":format!("{:x}", Sha256::digest([]))});
    assert!(!client
        .post(&url)
        .json(&offer)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    pair(&first, &second).await;
    second
        .engine
        .forget_peer(first.identity.fingerprint.clone())
        .unwrap();
    assert!(!client
        .post(&url)
        .json(&offer)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(snapshot(&second.engine)["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    first.stop().await;
    second.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Interactive native UI fixture controlled by local test files"]
async fn native_peer_fixture() {
    let root = std::env::var_os("LANLINK_FIXTURE_DIR")
        .expect("LANLINK_FIXTURE_DIR must be a test directory");
    let root = Path::new(&root).canonicalize().unwrap();
    let device = Device::start("P2 原生验证对端").await;
    fs::write(
        root.join("peer.json"),
        serde_json::to_vec(
            &json!({"port":device.port(),"fingerprint":device.identity.fingerprint}),
        )
        .unwrap(),
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "原生测试对端超时");
        if let Ok(bytes) = fs::read(root.join("command.json")) {
            let command: Value = serde_json::from_slice(&bytes).unwrap();
            fs::remove_file(root.join("command.json")).unwrap();
            let result = match command["action"].as_str().unwrap() {
                "snapshot" => snapshot(&device.engine),
                "connect" => {
                    let peer = device
                        .engine
                        .connect(
                            "127.0.0.1".into(),
                            command["port"].as_u64().unwrap().try_into().unwrap(),
                            command["fingerprint"].as_str().unwrap().into(),
                        )
                        .await
                        .unwrap();
                    json!({"peer":peer})
                }
                "forget" => {
                    device
                        .engine
                        .forget_peer(command["fingerprint"].as_str().unwrap().into())
                        .unwrap();
                    json!({"ok":true})
                }
                "approvePair" => {
                    let value = snapshot(&device.engine);
                    let request = value["pairRequests"]
                        .as_array()
                        .unwrap()
                        .first()
                        .expect("No pending pairing request");
                    assert_eq!(request["fingerprint"], command["fingerprint"]);
                    device
                        .engine
                        .respond_pair(request["id"].as_str().unwrap().into(), true)
                        .unwrap();
                    json!({"ok":true})
                }
                "accept" | "reject" => {
                    device
                        .engine
                        .respond_transfer(
                            command["id"].as_str().unwrap().into(),
                            command["action"] == "accept",
                            Some(root.to_str().unwrap().into()),
                        )
                        .unwrap();
                    json!({"ok":true})
                }
                "send" => {
                    json!({"id": device.engine.send(command["fingerprint"].as_str().unwrap().into(), command["path"].as_str().unwrap().into()).unwrap()})
                }
                "stop" => break,
                action => panic!("Unexpected fixture command: {action}"),
            };
            let temporary = root.join("reply.tmp");
            fs::write(
                &temporary,
                serde_json::to_vec(&json!({"sequence":command["sequence"],"result":result}))
                    .unwrap(),
            )
            .unwrap();
            fs::rename(temporary, root.join("reply.json")).unwrap();
        }
        sleep(Duration::from_millis(30)).await;
    }
    device.stop().await;
}
