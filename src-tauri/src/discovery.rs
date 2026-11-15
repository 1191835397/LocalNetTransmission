//! Untrusted mDNS hints only: discovery never changes trust or opens a peer connection.

use mdns_sd::{
    DaemonEvent, IfKind, IfPredicate, Receiver, ResolvedService, ServiceDaemon, ServiceEvent,
    ServiceInfo,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

const SERVICE_TYPE: &str = "_lanlink._tcp.local.";
const MAX_RECORDS: usize = 256;
const MAX_ADDRESSES: usize = 16;
const OBSERVATION_WINDOW: Duration = Duration::from_secs(5);
const VERIFY_INTERVAL: Duration = Duration::from_secs(15);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiscoveryState {
    Running,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDevice {
    pub fingerprint: String,
    pub name: String,
    pub platform: String,
    pub addresses: Vec<String>,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySnapshot {
    pub state: DiscoveryState,
    /// A five-second observation window, not proof that discovery is complete.
    pub scanning: bool,
    pub devices: Vec<DiscoveredDevice>,
    pub error: Option<String>,
}

impl DiscoverySnapshot {
    pub fn unavailable(message: &str) -> Self {
        Self {
            state: DiscoveryState::Error,
            scanning: false,
            devices: Vec::new(),
            error: Some(message.to_owned()),
        }
    }
}

#[derive(Clone)]
struct Request {
    name: String,
    fingerprint: String,
    // The running TLS port is immutable, even if the saved port setting changes.
    port: u16,
    refresh: u64,
}

pub struct DiscoveryHandle {
    shared: Arc<Mutex<DiscoveryData>>,
    request: Request,
    commands: Option<watch::Sender<Request>>,
    task: Option<JoinHandle<()>>,
}

pub fn start(name: String, fingerprint: String, port: u16) -> DiscoveryHandle {
    let fingerprint = normalize_fingerprint(&fingerprint).unwrap_or(fingerprint);
    let mut handle = DiscoveryHandle {
        shared: Arc::new(Mutex::new(DiscoveryData::default())),
        request: Request {
            name,
            fingerprint,
            port,
            refresh: 0,
        },
        commands: None,
        task: None,
    };
    if let Err(error) = handle.ensure_worker() {
        lock(&handle.shared).fail(error);
    }
    handle
}

impl DiscoveryHandle {
    pub fn snapshot(&self) -> DiscoverySnapshot {
        let mut data = lock(&self.shared);
        if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            data.fail("局域网发现任务已停止，请重新发现。".into());
        }
        data.snapshot(Instant::now())
    }

    // The watch channel coalesces repeated requests; completion appears in snapshots.
    pub fn refresh(&mut self) -> Result<(), String> {
        self.request.refresh = self.request.refresh.wrapping_add(1);
        let result = self.submit();
        let mut data = lock(&self.shared);
        match &result {
            Err(error) => data.fail(error.clone()),
            Ok(()) if data.error.is_none() => {
                // Indicate pending work until the worker replaces the browse receiver.
                data.scanning_until = Some(Instant::now() + OBSERVATION_WINDOW);
            }
            Ok(()) => {}
        }
        result
    }

    /// Advertising errors must not roll back an already-saved device name.
    pub fn set_name(&mut self, name: String) {
        self.request.name = name;
        if let Err(error) = self.submit() {
            lock(&self.shared).fail(error);
        }
    }

    pub async fn shutdown(mut self) {
        // Closing the command channel makes the worker send goodbye and await the daemon.
        self.commands.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    fn submit(&mut self) -> Result<(), String> {
        self.ensure_worker()?;
        self.commands
            .as_ref()
            .ok_or("局域网发现任务未启动。")?
            .send(self.request.clone())
            .map_err(|_| "局域网发现命令通道已关闭，请重新发现。".into())
    }

    fn ensure_worker(&mut self) -> Result<(), String> {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        // Validate before creating any sockets. Invalid configuration stays recoverable.
        advertisement(&self.request)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "局域网发现异步运行环境不可用，请重新发现。".to_owned())?;
        self.commands.take();
        self.task.take();
        let (commands, receiver) = watch::channel(self.request.clone());
        lock(&self.shared).begin_round(Instant::now());
        self.task = Some(runtime.spawn(run(receiver, Arc::clone(&self.shared))));
        self.commands = Some(commands);
        Ok(())
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        // Let the detached worker send goodbye; DaemonGuard covers runtime cancellation.
        self.commands.take();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn normalize_fingerprint(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 95
        || !bytes.iter().enumerate().all(|(index, byte)| {
            if index % 3 == 2 {
                *byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(value.to_ascii_uppercase())
}

fn forbidden_character(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
}

fn valid_name(value: &str) -> Option<&str> {
    if value.len() > 256 || value.chars().any(forbidden_character) {
        return None;
    }
    let value = value.trim();
    (1..=64).contains(&value.chars().count()).then_some(value)
}

fn private_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && (ip.is_private() || ip.is_link_local())
}

fn private_interface() -> IfKind {
    IfKind::Predicate(IfPredicate::new(
        |interface| matches!(interface.ip(), IpAddr::V4(ip) if private_ipv4(ip)),
    ))
}

fn advertisement(request: &Request) -> Result<ServiceInfo, String> {
    let fingerprint = normalize_fingerprint(&request.fingerprint).ok_or("发现广播指纹无效。")?;
    let name = valid_name(&request.name).ok_or("发现广播名称须为 1–64 个字符且不含控制字符。")?;
    if request.port < 1024 {
        return Err("发现广播端口须在 1024–65535 之间。".into());
    }
    // TXT allows 255 bytes including "name="; truncate only the advertised UTF-8 value.
    let mut end = name.len().min(255 - "name=".len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let name = &name[..end];
    let hex = fingerprint.replace(':', "").to_ascii_lowercase();
    // Short DNS labels use 128 bits; identity and deduplication still use all 256 bits.
    let instance = format!("lanlink-{}", &hex[..32]);
    let hostname = format!("{instance}.local.");
    let properties = [
        ("name", name),
        ("fingerprint", fingerprint.as_str()),
        ("proto", "1"),
        ("platform", std::env::consts::OS),
    ];
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &hostname,
        "",
        request.port,
        &properties[..],
    )
    .map_err(|error| format!("无法构建设备发现广播：{error}"))?
    .enable_addr_auto();
    // Restrict auto-addresses as well as outbound interfaces, including future NICs.
    info.set_interfaces(vec![private_interface()]);
    Ok(info)
}

#[derive(Clone, Debug)]
struct Record {
    // Preserve wire spelling because mdns-sd's SRV lookup for verify is case-sensitive.
    fullname: String,
    fingerprint: String,
    name: String,
    platform: String,
    addresses: BTreeSet<Ipv4Addr>,
    port: u16,
}

fn fullname_key(fullname: &str) -> Option<String> {
    if fullname.len() > 255 || fullname.chars().any(forbidden_character) {
        return None;
    }
    let key = fullname.to_ascii_lowercase();
    let instance = key.strip_suffix(SERVICE_TYPE)?.strip_suffix('.')?;
    (!instance.is_empty()).then_some(key)
}

fn txt<'a>(service: &'a ResolvedService, key: &str) -> Option<&'a str> {
    // Do not accept lossy UTF-8 from an untrusted TXT record.
    let bytes = service.get_property_val(key)??;
    if key.len() + 1 + bytes.len() > 255 {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn limit_addresses(addresses: impl IntoIterator<Item = Ipv4Addr>) -> BTreeSet<Ipv4Addr> {
    let mut result = BTreeSet::new();
    for address in addresses {
        if private_ipv4(address) {
            result.insert(address);
            if result.len() > MAX_ADDRESSES {
                result.pop_last();
            }
        }
    }
    result
}

fn parse_resolved(service: &ResolvedService, own_fingerprint: &str) -> Option<Record> {
    if !service.ty_domain.eq_ignore_ascii_case(SERVICE_TYPE)
        || fullname_key(service.get_fullname()).is_none()
        || txt(service, "proto")? != "1"
        || service.get_port() < 1024
    {
        return None;
    }
    let fingerprint = normalize_fingerprint(txt(service, "fingerprint")?)?;
    if fingerprint.eq_ignore_ascii_case(own_fingerprint) {
        return None;
    }
    let name = valid_name(txt(service, "name")?)?.to_owned();
    let platform = txt(service, "platform")?;
    if platform.is_empty()
        || platform.len() > 32
        || !platform
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return None;
    }
    let addresses = limit_addresses(service.get_addresses_v4());
    if addresses.is_empty() {
        return None;
    }
    Some(Record {
        fullname: service.get_fullname().to_owned(),
        fingerprint,
        name,
        platform: platform.to_ascii_lowercase(),
        addresses,
        port: service.get_port(),
    })
}

#[derive(Default)]
struct DiscoveryData {
    // Records are keyed by DNS fullname, not by display name or truncated identity.
    records: BTreeMap<String, Record>,
    scanning_until: Option<Instant>,
    error: Option<String>,
}

impl DiscoveryData {
    fn begin_round(&mut self, now: Instant) {
        self.records.clear();
        self.error = None;
        self.scanning_until = Some(now + OBSERVATION_WINDOW);
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message);
        self.scanning_until = None;
        self.records.clear();
    }

    fn event(&mut self, event: ServiceEvent, own_fingerprint: &str) -> Result<(), String> {
        match event {
            ServiceEvent::ServiceResolved(service) => {
                if let Some(key) = fullname_key(service.get_fullname()) {
                    // Even invalid or self updates must invalidate the instance's old addresses.
                    self.records.remove(&key);
                    if let Some(record) = parse_resolved(&service, own_fingerprint) {
                        self.records.insert(key, record);
                        if self.records.len() > MAX_RECORDS {
                            // Deterministic admission under flooding, independent of arrival order.
                            self.records.pop_last();
                        }
                    }
                }
            }
            ServiceEvent::ServiceRemoved(service_type, fullname) => {
                if service_type.eq_ignore_ascii_case(SERVICE_TYPE) {
                    if let Some(key) = fullname_key(&fullname) {
                        self.records.remove(&key);
                    }
                }
            }
            ServiceEvent::SearchStopped(_) => {
                return Err("局域网发现查询意外停止，请重新发现。".into());
            }
            _ => {}
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> DiscoverySnapshot {
        let mut devices: BTreeMap<&str, (DiscoveredDevice, BTreeSet<Ipv4Addr>)> = BTreeMap::new();
        // Lexicographically first fullname supplies metadata on conflicting advertisements.
        for record in self.records.values() {
            let (device, addresses) = devices.entry(&record.fingerprint).or_insert_with(|| {
                (
                    DiscoveredDevice {
                        fingerprint: record.fingerprint.clone(),
                        name: record.name.clone(),
                        platform: record.platform.clone(),
                        addresses: Vec::new(),
                        port: record.port,
                    },
                    BTreeSet::new(),
                )
            });
            if device.port == record.port {
                *addresses = limit_addresses(addresses.iter().chain(&record.addresses).copied());
            }
        }
        DiscoverySnapshot {
            state: if self.error.is_some() {
                DiscoveryState::Error
            } else {
                DiscoveryState::Running
            },
            scanning: self.scanning_until.is_some_and(|deadline| now < deadline),
            devices: devices
                .into_values()
                .map(|(mut device, addresses)| {
                    device.addresses = addresses.into_iter().map(|ip| ip.to_string()).collect();
                    device
                })
                .collect(),
            error: self.error.clone(),
        }
    }
}

// Verification bursts can fill the bounded daemon queue; retry only transient backpressure.
async fn command<T>(mut send: impl FnMut() -> mdns_sd::Result<T>) -> Result<T, String> {
    let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match send() {
            Ok(value) => return Ok(value),
            Err(mdns_sd::Error::Again) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(format!("局域网发现服务错误：{error}")),
        }
    }
}

struct DaemonGuard {
    daemon: ServiceDaemon,
    shutdown_sent: bool,
}

impl DaemonGuard {
    async fn close(mut self) {
        if let Ok(receiver) = command(|| self.daemon.shutdown()).await {
            self.shutdown_sent = true;
            // The daemon sends goodbye before acknowledging shutdown. Never abort it first.
            let _ = tokio::time::timeout(Duration::from_secs(3), receiver.recv_async()).await;
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if !self.shutdown_sent && matches!(self.daemon.shutdown(), Err(mdns_sd::Error::Again)) {
            // Runtime teardown may cancel the worker with a full queue; retry without Tokio.
            let daemon = self.daemon.clone();
            let _ = std::thread::Builder::new()
                .name("lanlink-mdns-cleanup".into())
                .spawn(move || {
                    let deadline = Instant::now() + COMMAND_TIMEOUT;
                    loop {
                        match daemon.shutdown() {
                            Err(mdns_sd::Error::Again) if Instant::now() < deadline => {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Ok(receiver) => {
                                let _ = receiver.recv_timeout(Duration::from_secs(3));
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                });
        }
    }
}

struct Session {
    guard: DaemonGuard,
    events: Receiver<ServiceEvent>,
    monitor: Receiver<DaemonEvent>,
}

impl Session {
    async fn open(request: &Request) -> Result<Self, String> {
        let info = advertisement(request)?;
        let guard = DaemonGuard {
            daemon: ServiceDaemon::new().map_err(|error| format!("无法启动局域网发现：{error}"))?,
            shutdown_sent: false,
        };
        let result = async {
            command(|| {
                guard.daemon.disable_interface(vec![
                    IfKind::IPv6,
                    IfKind::LoopbackV4,
                    IfKind::LoopbackV6,
                    IfKind::Predicate(IfPredicate::new(
                        |interface| !matches!(interface.ip(), IpAddr::V4(ip) if private_ipv4(ip)),
                    )),
                ])
            })
            .await?;
            let monitor = command(|| guard.daemon.monitor()).await?;
            command(|| guard.daemon.register(info.clone())).await?;
            let events = command(|| guard.daemon.browse(SERVICE_TYPE)).await?;
            Ok::<_, String>((monitor, events))
        }
        .await;
        match result {
            Ok((monitor, events)) => Ok(Self {
                guard,
                events,
                monitor,
            }),
            Err(error) => {
                // Initialization may already have registered a service.
                guard.close().await;
                Err(error)
            }
        }
    }

    async fn refresh(&mut self) -> Result<(), String> {
        command(|| self.guard.daemon.stop_browse(SERVICE_TYPE)).await?;
        // FIFO stop clears cached records; replacing the receiver excludes stale events.
        self.events = command(|| self.guard.daemon.browse(SERVICE_TYPE)).await?;
        Ok(())
    }
}

async fn apply_request(
    session: &mut Option<Session>,
    request: &Request,
    refresh: bool,
    shared: &Mutex<DiscoveryData>,
) -> Result<(), String> {
    let recovering = lock(shared).error.is_some();
    if recovering {
        close_session(session).await;
    }
    if let Some(active) = session {
        let info = advertisement(request)?;
        command(|| active.guard.daemon.register(info.clone())).await?;
        if refresh {
            active.refresh().await?;
            lock(shared).begin_round(Instant::now());
        }
    } else {
        *session = Some(Session::open(request).await?);
        lock(shared).begin_round(Instant::now());
    }
    Ok(())
}

async fn close_session(session: &mut Option<Session>) {
    if let Some(active) = session.take() {
        active.guard.close().await;
    }
}

enum Input {
    Command(Result<(), watch::error::RecvError>),
    Service(Result<ServiceEvent, mdns_sd::RecvError>),
    Monitor(Result<DaemonEvent, mdns_sd::RecvError>),
    Verify,
}

async fn run(mut commands: watch::Receiver<Request>, shared: Arc<Mutex<DiscoveryData>>) {
    let mut request = commands.borrow_and_update().clone();
    let mut session = None;
    // If the handle was dropped before the task's first poll, do not open sockets.
    if commands.has_changed().is_err() {
        return;
    }
    if let Err(error) = apply_request(&mut session, &request, true, &shared).await {
        lock(&shared).fail(error);
        close_session(&mut session).await;
    }
    let mut verify = tokio::time::interval_at(
        tokio::time::Instant::now() + VERIFY_INTERVAL,
        VERIFY_INTERVAL,
    );
    verify.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let input = if let Some(active) = &session {
            tokio::select! {
                // A queued refresh wins over buffered events from the previous receiver.
                biased;
                result = commands.changed() => Input::Command(result),
                _ = verify.tick() => Input::Verify,
                result = active.monitor.recv_async() => Input::Monitor(result),
                result = active.events.recv_async() => Input::Service(result),
            }
        } else {
            Input::Command(commands.changed().await)
        };
        let result = match input {
            Input::Command(Err(_)) => break,
            Input::Command(Ok(())) => {
                let next = commands.borrow_and_update().clone();
                let refresh = next.refresh != request.refresh;
                let changed = refresh || next.name != request.name;
                request = next;
                if changed || session.is_none() {
                    apply_request(&mut session, &request, refresh, &shared).await
                } else {
                    Ok(())
                }
            }
            Input::Service(Ok(event)) => lock(&shared).event(event, &request.fingerprint),
            Input::Service(Err(_)) => Err("局域网发现事件通道已关闭，请重新发现。".into()),
            Input::Monitor(Ok(DaemonEvent::Error(error))) => {
                Err(format!("局域网发现服务错误：{error}"))
            }
            Input::Monitor(Err(_)) => Err("局域网发现监控通道已关闭，请重新发现。".into()),
            Input::Monitor(Ok(_)) => Ok(()),
            Input::Verify => {
                let names: Vec<_> = lock(&shared)
                    .records
                    .values()
                    .map(|record| record.fullname.clone())
                    .collect();
                let mut result = Ok(());
                if let Some(active) = &session {
                    for fullname in names {
                        result = command(|| {
                            active.guard.daemon.verify(fullname.clone(), VERIFY_TIMEOUT)
                        })
                        .await;
                        if result.is_err() || commands.has_changed().unwrap_or(true) {
                            break;
                        }
                    }
                }
                // Successful verification may not emit ServiceResolved, so silence is not expiry.
                result
            }
        };
        if let Err(error) = result {
            lock(&shared).fail(error);
            close_session(&mut session).await;
        }
    }
    close_session(&mut session).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8) -> String {
        (0..32)
            .map(|_| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    fn request(name: &str) -> Request {
        Request {
            name: name.into(),
            fingerprint: fingerprint(0xAA),
            port: 53318,
            refresh: 0,
        }
    }

    fn resolved(instance: &str, fingerprint: &str, ips: &str) -> ResolvedService {
        ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            "test-device.local.",
            ips,
            53318,
            &[
                ("name", "同名设备"),
                ("fingerprint", fingerprint),
                ("proto", "1"),
                ("platform", "windows"),
            ][..],
        )
        .unwrap()
        .as_resolved_service()
    }

    fn property(service: &mut ResolvedService, key: &str, value: &str) {
        let mut properties: BTreeMap<String, String> = ["name", "fingerprint", "proto", "platform"]
            .into_iter()
            .map(|key| (key.to_owned(), txt(service, key).unwrap().to_owned()))
            .collect();
        properties.insert(key.into(), value.into());
        let pairs: Vec<_> = properties.into_iter().collect();
        service.txt_properties = mdns_sd::IntoTxtProperties::into_txt_properties(&pairs[..]);
    }

    fn accept(data: &mut DiscoveryData, service: ResolvedService) {
        data.event(
            ServiceEvent::ServiceResolved(Box::new(service)),
            &fingerprint(0),
        )
        .unwrap();
    }

    #[test]
    fn advertisement_has_short_labels_and_complete_identity() {
        let request = request("工作电脑");
        let ad = advertisement(&request).unwrap();
        assert!(ad.is_addr_auto());
        assert!(ad.get_hostname().split('.').all(|label| label.len() <= 63));
        assert!(ad.get_fullname().split('.').next().unwrap().len() <= 63);
        assert_eq!(ad.get_port(), 53318);
        assert_eq!(
            ad.get_property_val_str("fingerprint"),
            Some(request.fingerprint.as_str())
        );
        assert_eq!(ad.get_property_val_str("proto"), Some("1"));
        assert_eq!(
            ad.get_property_val_str("platform"),
            Some(std::env::consts::OS)
        );
        let mut renamed = request.clone();
        renamed.name = "新名称".into();
        let renamed = advertisement(&renamed).unwrap();
        assert_eq!(renamed.get_fullname(), ad.get_fullname());
        assert_eq!(renamed.get_hostname(), ad.get_hostname());
        assert_eq!(renamed.get_port(), ad.get_port());
        assert_eq!(renamed.get_property_val_str("name"), Some("新名称"));
    }

    #[test]
    fn long_unicode_advertisement_truncates_only_at_utf8_boundary() {
        let name = "\u{20000}".repeat(64);
        let request = request(&name);
        let ad = advertisement(&request).unwrap();
        let advertised = ad.get_property_val_str("name").unwrap();
        assert_eq!(advertised.chars().count(), 62);
        assert!(advertised.len() + "name=".len() <= 255);
        assert_eq!(request.name, name);
    }

    #[test]
    fn complete_fingerprints_are_normalized_and_self_is_excluded() {
        let lower = fingerprint(0xAB).to_ascii_lowercase();
        assert_eq!(normalize_fingerprint(&lower), Some(fingerprint(0xAB)));
        for invalid in ["AB".repeat(32), fingerprint(1) + ":00", "GG:".repeat(32)] {
            assert!(normalize_fingerprint(&invalid).is_none());
        }
        let service = resolved("peer", &lower, "192.168.1.2");
        assert!(parse_resolved(&service, &fingerprint(0xAB)).is_none());
        assert_eq!(
            parse_resolved(&service, &fingerprint(0))
                .unwrap()
                .fingerprint,
            fingerprint(0xAB)
        );
    }

    #[test]
    fn only_private_and_link_local_ipv4_are_kept_in_numeric_order() {
        let service = resolved(
            "peer",
            &fingerprint(1),
            "192.168.1.10,192.168.1.2,10.0.0.2,172.16.0.2,172.31.255.254,169.254.2.3,127.0.0.1,0.0.0.0,8.8.8.8,172.32.0.1,100.64.0.1,224.0.0.1,255.255.255.255,::1,fe80::1,fd00::1",
        );
        let record = parse_resolved(&service, &fingerprint(0)).unwrap();
        let addresses: Vec<_> = record.addresses.iter().map(ToString::to_string).collect();
        assert_eq!(
            addresses,
            [
                "10.0.0.2",
                "169.254.2.3",
                "172.16.0.2",
                "172.31.255.254",
                "192.168.1.2",
                "192.168.1.10"
            ]
        );
        for ips in ["8.8.8.8", "127.0.0.1", "::1", "fd00::1", "0.0.0.0"] {
            assert!(
                parse_resolved(&resolved("peer", &fingerprint(1), ips), &fingerprint(0)).is_none()
            );
        }
    }

    #[test]
    fn untrusted_protocol_names_platform_and_ports_are_validated() {
        let base = resolved("peer", &fingerprint(1), "192.168.1.2");
        for (key, value) in [
            ("proto", "2"),
            ("proto", "01"),
            ("proto", ""),
            ("fingerprint", "not-a-fingerprint"),
            ("name", " "),
            ("name", "line\nfeed"),
            ("name", "bad\u{202e}name"),
            ("name", "bad\u{2066}name"),
            ("name", "bad\u{061c}name"),
            ("platform", ""),
            ("platform", "bad\nplatform"),
            ("platform", "<script>"),
        ] {
            let mut service = base.clone();
            property(&mut service, key, value);
            assert!(
                parse_resolved(&service, &fingerprint(0)).is_none(),
                "{key}: {value:?}"
            );
        }
        let mut service = base.clone();
        property(&mut service, "name", &"a".repeat(65));
        assert!(parse_resolved(&service, &fingerprint(0)).is_none());
        property(&mut service, "name", &"界".repeat(64));
        assert!(parse_resolved(&service, &fingerprint(0)).is_some());
        for port in [0, 1, 1023] {
            service.port = port;
            assert!(parse_resolved(&service, &fingerprint(0)).is_none());
        }
        for port in [1024, 65535] {
            service.port = port;
            assert!(parse_resolved(&service, &fingerprint(0)).is_some());
        }
        service.ty_domain = "_other._tcp.local.".into();
        assert!(parse_resolved(&service, &fingerprint(0)).is_none());
        service = base;
        service.fullname = "peer._other._tcp.local.".into();
        assert!(parse_resolved(&service, &fingerprint(0)).is_none());
    }

    #[test]
    fn missing_or_invalid_utf8_txt_is_rejected() {
        let mut service = resolved("peer", &fingerprint(1), "10.0.0.2");
        let properties: Vec<mdns_sd::TxtProperty> = [
            ("name", vec![0xff]),
            ("fingerprint", fingerprint(1).into_bytes()),
            ("proto", b"1".to_vec()),
            ("platform", b"windows".to_vec()),
        ]
        .into_iter()
        .map(mdns_sd::TxtProperty::from)
        .collect();
        service.txt_properties = mdns_sd::IntoTxtProperties::into_txt_properties(properties);
        assert!(parse_resolved(&service, &fingerprint(0)).is_none());
        service.txt_properties =
            mdns_sd::IntoTxtProperties::into_txt_properties(&[("name", "peer")][..]);
        assert!(parse_resolved(&service, &fingerprint(0)).is_none());
    }

    #[test]
    fn deduplication_is_by_full_fingerprint_not_name_or_short_prefix() {
        let first = fingerprint(1);
        let mut second = first.clone();
        second.replace_range(93..95, "02");
        let services = [
            resolved("a", &first, "10.0.0.2,192.168.1.2"),
            resolved("b", &first.to_ascii_lowercase(), "192.168.1.2,192.168.1.10"),
            resolved("c", &second, "10.0.0.3"),
        ];
        let mut forward = DiscoveryData::default();
        let mut reverse = DiscoveryData::default();
        for service in services.iter().cloned() {
            accept(&mut forward, service);
        }
        for service in services.iter().rev().cloned() {
            accept(&mut reverse, service);
        }
        let devices = forward.snapshot(Instant::now()).devices;
        assert_eq!(devices, reverse.snapshot(Instant::now()).devices);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, devices[1].name);
        assert_eq!(
            devices[0].addresses,
            ["10.0.0.2", "192.168.1.2", "192.168.1.10"]
        );
    }

    #[test]
    fn resolved_replaces_addresses_and_removed_only_removes_its_instance() {
        let mut data = DiscoveryData::default();
        accept(
            &mut data,
            resolved("a", &fingerprint(1), "10.0.0.1,10.0.0.2"),
        );
        accept(&mut data, resolved("b", &fingerprint(1), "10.0.0.3"));
        accept(&mut data, resolved("A", &fingerprint(1), "10.0.0.4"));
        assert_eq!(
            data.snapshot(Instant::now()).devices[0].addresses,
            ["10.0.0.3", "10.0.0.4"]
        );
        data.event(
            ServiceEvent::ServiceRemoved(
                SERVICE_TYPE.into(),
                format!("A.{SERVICE_TYPE}").to_uppercase(),
            ),
            &fingerprint(0),
        )
        .unwrap();
        assert_eq!(
            data.snapshot(Instant::now()).devices[0].addresses,
            ["10.0.0.3"]
        );
        accept(&mut data, resolved("b", &fingerprint(0), "10.0.0.5"));
        assert!(data.records.is_empty());
        accept(&mut data, resolved("b", &fingerprint(1), "10.0.0.3"));
        accept(&mut data, resolved("b", &fingerprint(1), "8.8.8.8"));
        assert!(data.records.is_empty());
    }

    #[test]
    fn verification_preserves_wire_fullname_case_while_removal_is_case_insensitive() {
        let mut data = DiscoveryData::default();
        let service = resolved("MixedCase", &fingerprint(1), "10.0.0.2");
        let original = service.get_fullname().to_owned();
        accept(&mut data, service);
        assert_eq!(data.records.values().next().unwrap().fullname, original);
        data.event(
            ServiceEvent::ServiceRemoved(SERVICE_TYPE.into(), original.to_ascii_uppercase()),
            &fingerprint(0),
        )
        .unwrap();
        assert!(data.records.is_empty());
    }

    #[test]
    fn conflicting_ports_do_not_create_invalid_address_port_pairs() {
        let mut data = DiscoveryData::default();
        accept(&mut data, resolved("a", &fingerprint(1), "10.0.0.2"));
        let mut other = resolved("b", &fingerprint(1), "10.0.0.3");
        other.port = 60000;
        accept(&mut data, other);
        let devices = data.snapshot(Instant::now()).devices;
        assert_eq!(devices[0].port, 53318);
        assert_eq!(devices[0].addresses, ["10.0.0.2"]);
    }

    #[test]
    fn record_and_address_limits_are_bounded_and_deterministic() {
        let mut forward = DiscoveryData::default();
        let mut reverse = DiscoveryData::default();
        for index in 0..300 {
            accept(
                &mut forward,
                resolved(&format!("peer-{index:03}"), &fingerprint(1), "10.0.0.1"),
            );
        }
        for index in (0..300).rev() {
            accept(
                &mut reverse,
                resolved(&format!("peer-{index:03}"), &fingerprint(1), "10.0.0.1"),
            );
        }
        assert_eq!(forward.records.len(), MAX_RECORDS);
        assert_eq!(
            forward.records.keys().collect::<Vec<_>>(),
            reverse.records.keys().collect::<Vec<_>>()
        );
        accept(
            &mut forward,
            resolved("peer-000", &fingerprint(1), "10.0.0.9"),
        );
        assert!(forward.records[&format!("peer-000.{SERVICE_TYPE}")]
            .addresses
            .contains(&Ipv4Addr::new(10, 0, 0, 9)));
        let addresses = (1..=64).map(|n| Ipv4Addr::new(10, 0, 0, n));
        assert_eq!(
            limit_addresses(addresses.clone()),
            limit_addresses(addresses.rev())
        );
        assert_eq!(
            limit_addresses((1..=64).map(|n| Ipv4Addr::new(10, 0, 0, n))).len(),
            MAX_ADDRESSES
        );
        let mut aggregate = DiscoveryData::default();
        for n in 1..=32 {
            accept(
                &mut aggregate,
                resolved(
                    &format!("peer-{n}"),
                    &fingerprint(1),
                    &format!("10.0.0.{n}"),
                ),
            );
        }
        assert_eq!(
            aggregate.snapshot(Instant::now()).devices[0]
                .addresses
                .len(),
            MAX_ADDRESSES
        );
    }

    #[test]
    fn observation_window_does_not_expire_live_devices_and_refresh_clears_records() {
        let now = Instant::now();
        let mut data = DiscoveryData::default();
        data.begin_round(now);
        accept(&mut data, resolved("a", &fingerprint(1), "10.0.0.2"));
        assert!(data.snapshot(now + Duration::from_millis(4999)).scanning);
        assert!(!data.snapshot(now + OBSERVATION_WINDOW).scanning);
        assert_eq!(
            data.snapshot(now + Duration::from_secs(3600)).devices.len(),
            1
        );
        data.begin_round(now + Duration::from_secs(3600));
        assert!(data.snapshot(now + Duration::from_secs(3600)).scanning);
        assert!(data.records.is_empty());
        data.fail("monitor failed".into());
        let snapshot = data.snapshot(now);
        assert_eq!(snapshot.state, DiscoveryState::Error);
        assert!(!snapshot.scanning);
        assert_eq!(snapshot.error.as_deref(), Some("monitor failed"));
        data.begin_round(now);
        assert!(data.snapshot(now).error.is_none());
    }

    #[test]
    fn snapshots_have_the_public_serialization_contract() {
        let unavailable =
            serde_json::to_value(DiscoverySnapshot::unavailable("TLS unavailable")).unwrap();
        assert_eq!(
            unavailable,
            serde_json::json!({
                "state": "error", "scanning": false, "devices": [], "error": "TLS unavailable"
            })
        );
        let mut data = DiscoveryData::default();
        accept(&mut data, resolved("a", &fingerprint(1), "10.0.0.2"));
        let running = serde_json::to_value(data.snapshot(Instant::now())).unwrap();
        assert_eq!(running["state"], "running");
        assert!(running["error"].is_null());
        assert_eq!(running["devices"][0]["port"], 53318);
        assert_eq!(running["devices"][0]["fingerprint"], fingerprint(1));
    }

    #[test]
    fn absent_runtime_and_invalid_identity_fail_without_network_io() {
        let mut handle = start("测试设备".into(), fingerprint(1), 53318);
        assert_eq!(handle.snapshot().state, DiscoveryState::Error);
        assert!(handle.refresh().is_err());
        handle.set_name("已保存的新名称".into());
        assert_eq!(handle.request.name, "已保存的新名称");
        assert_eq!(handle.request.port, 53318);
        let invalid = start("测试设备".into(), "invalid".into(), 53318);
        assert!(invalid.snapshot().error.unwrap().contains("指纹"));
    }

    // No ServiceDaemon is created in these lifecycle tests.
    fn fake_handle() -> (DiscoveryHandle, tokio::sync::oneshot::Receiver<()>) {
        let (commands, mut receiver) = watch::channel(request("test"));
        let (done, completion) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            while receiver.changed().await.is_ok() {}
            let _ = done.send(());
        });
        (
            DiscoveryHandle {
                shared: Arc::new(Mutex::new(DiscoveryData::default())),
                request: request("test"),
                commands: Some(commands),
                task: Some(task),
            },
            completion,
        )
    }

    #[tokio::test]
    async fn explicit_shutdown_waits_and_drop_signals_instead_of_aborting() {
        let (handle, done) = fake_handle();
        handle.shutdown().await;
        done.await.unwrap();
        let (handle, done) = fake_handle();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), done)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn requests_coalesce_and_retain_actual_port() {
        let (commands, mut receiver) = watch::channel(request("old"));
        let mut handle = DiscoveryHandle {
            shared: Arc::new(Mutex::new(DiscoveryData::default())),
            request: request("old"),
            commands: Some(commands),
            task: Some(tokio::spawn(std::future::pending())),
        };
        for _ in 0..100 {
            handle.refresh().unwrap();
        }
        handle.set_name("new".into());
        receiver.changed().await.unwrap();
        let latest = receiver.borrow_and_update().clone();
        assert_eq!(latest.refresh, 100);
        assert_eq!(latest.name, "new");
        assert_eq!(latest.port, 53318);
        assert!(!receiver.has_changed().unwrap());
        // This synthetic pending task owns no daemon and only exists for queue testing.
        handle.task.take().unwrap().abort();
    }

    #[tokio::test]
    async fn invalid_configuration_does_not_revert_the_saved_name() {
        let mut handle = start("".into(), fingerprint(1), 53318);
        assert_eq!(handle.snapshot().state, DiscoveryState::Error);
        handle.set_name("bad\u{202e}name".into());
        assert!(handle.refresh().is_err());
        assert_eq!(handle.request.name, "bad\u{202e}name");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn worker_reports_initialization_failure_and_processes_retry_without_sockets() {
        let shared = Arc::new(Mutex::new(DiscoveryData::default()));
        let (commands, receiver) = watch::channel(request(""));
        let task = tokio::spawn(run(receiver, Arc::clone(&shared)));
        async fn wait_for_error(shared: &Mutex<DiscoveryData>, expected: &str) {
            tokio::time::timeout(Duration::from_secs(1), async {
                while !lock(shared)
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains(expected))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        wait_for_error(&shared, "名称").await;
        // The second initialization also fails before ServiceDaemon::new: no packets.
        let mut retry = request("valid name");
        retry.port = 1;
        retry.refresh = 1;
        commands.send(retry).unwrap();
        wait_for_error(&shared, "端口").await;
        drop(commands);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn closed_command_channel_is_reported_and_unpolled_drop_does_not_start_daemon() {
        let (mut handle, done) = fake_handle();
        handle.task.take().unwrap().abort();
        let _ = done.await;
        // Keep a synthetic task alive so submit tests the closed channel, not a new daemon.
        handle.task = Some(tokio::spawn(std::future::pending()));
        assert!(handle.refresh().unwrap_err().contains("通道"));
        assert_eq!(handle.snapshot().state, DiscoveryState::Error);
        handle.task.take().unwrap().abort();

        let shared = Arc::new(Mutex::new(DiscoveryData::default()));
        let (commands, receiver) = watch::channel(request("valid name"));
        drop(commands);
        // A valid request would start sockets unless run checks for closure first.
        run(receiver, shared).await;
    }

    /// Opt-in only: publishes synthetic identities on the LAN, never reads real settings.
    #[tokio::test]
    #[ignore = "sends real mDNS traffic; run explicitly on a private IPv4 LAN"]
    async fn real_mdns_two_daemons_discover_refresh_rename_and_goodbye() {
        let first_id = fingerprint(0xE1);
        let second_id = fingerprint(0xE2);
        let mut first = start("mDNS test first".into(), first_id.clone(), 55318);
        let mut second = start("mDNS test second".into(), second_id.clone(), 55319);
        async fn wait_for(handle: &DiscoveryHandle, id: &str, name: &str, present: bool) {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let snapshot = handle.snapshot();
                    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
                    if snapshot
                        .devices
                        .iter()
                        .any(|d| d.fingerprint == id && d.name == name)
                        == present
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("mDNS observation timed out");
        }
        wait_for(&first, &second_id, "mDNS test second", true).await;
        wait_for(&second, &first_id, "mDNS test first", true).await;
        assert!(!first
            .snapshot()
            .devices
            .iter()
            .any(|d| d.fingerprint == first_id));
        first.refresh().unwrap();
        let queued_deadline = lock(&first.shared).scanning_until;
        // Await the worker's new round so cached results cannot satisfy rediscovery.
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock(&first.shared).scanning_until == queued_deadline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mDNS refresh was not processed");
        wait_for(&first, &second_id, "mDNS test second", true).await;
        second.set_name("mDNS test renamed".into());
        wait_for(&first, &second_id, "mDNS test renamed", true).await;
        assert_eq!(
            first
                .snapshot()
                .devices
                .iter()
                .find(|d| d.fingerprint == second_id)
                .unwrap()
                .port,
            55319
        );
        second.shutdown().await;
        wait_for(&first, &second_id, "mDNS test renamed", false).await;
        first.shutdown().await;
    }
}
