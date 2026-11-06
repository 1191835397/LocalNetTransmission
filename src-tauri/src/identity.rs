use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, SecondsFormat, Utc};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const CREDENTIAL_SERVICE: &str = "com.localnet.transmission";
const CREDENTIAL_ACCOUNT: &str = "device-identity-v1";
const MAX_SECRET_BYTES: usize = 32 * 1024;

// Intentionally neither Debug nor Serialize: the private key never crosses IPC or logs.
pub struct Identity {
    pub certificate: CertificateDer<'static>,
    private_key: PrivateKeyDer<'static>,
    pub fingerprint: String,
    pub created_at: String,
}

#[derive(Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIdentity {
    schema_version: u8,
    certificate_der: String,
    private_key_der: String,
    created_at: String,
}

impl Identity {
    pub fn id(&self) -> String {
        format!(
            "lanlink-{}",
            self.fingerprint.replace(':', "").to_lowercase()
        )
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        self.private_key.clone_key()
    }
}

pub fn storage_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows Credential Manager"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS Keychain"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "当前平台未接入系统身份存储"
    }
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn generate() -> Result<Identity, String> {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .map_err(|_| "生成本机 TLS 身份失败。")?;
    let certificate = cert.der().clone();
    let private_key = PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into();
    Ok(Identity {
        fingerprint: fingerprint(certificate.as_ref()),
        certificate,
        private_key,
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    })
}

fn encode(identity: &Identity) -> Result<Zeroizing<Vec<u8>>, String> {
    let stored = StoredIdentity {
        schema_version: 1,
        certificate_der: STANDARD.encode(identity.certificate.as_ref()),
        private_key_der: STANDARD.encode(identity.private_key.secret_der()),
        created_at: identity.created_at.clone(),
    };
    serde_json::to_vec(&stored)
        .map(Zeroizing::new)
        .map_err(|_| "序列化系统身份凭据失败。".into())
}

fn decode(secret: &[u8]) -> Result<Identity, String> {
    if secret.len() > MAX_SECRET_BYTES {
        return Err("系统身份凭据过大，拒绝加载且不会覆盖。".into());
    }
    // Do not include deserializer errors: a malformed secret may contain private key data.
    let stored: StoredIdentity = serde_json::from_slice(secret)
        .map_err(|_| "系统身份凭据损坏，未重新生成或覆盖；请检查系统凭据。")?;
    if stored.schema_version != 1 {
        return Err("系统身份凭据版本不受支持，未覆盖身份。".into());
    }
    DateTime::parse_from_rfc3339(&stored.created_at)
        .map_err(|_| "系统身份创建时间无效，未覆盖身份。")?;
    let certificate = CertificateDer::from(
        STANDARD
            .decode(&stored.certificate_der)
            .map_err(|_| "系统身份中的证书编码无效，未覆盖身份。")?,
    );
    let private_key: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(
        STANDARD
            .decode(&stored.private_key_der)
            .map_err(|_| "系统身份中的私钥编码无效，未覆盖身份。")?,
    )
    .into();
    rustls::sign::CertifiedKey::from_der(
        vec![certificate.clone()],
        private_key.clone_key(),
        &rustls::crypto::ring::default_provider(),
    )
    .map_err(|_| "系统身份的证书或私钥无效或不匹配，未覆盖身份。")?;
    Ok(Identity {
        fingerprint: fingerprint(certificate.as_ref()),
        certificate,
        private_key,
        created_at: stored.created_at.clone(),
    })
}

enum CredentialReadError {
    Missing,
    Unavailable(String),
}

trait CredentialStore {
    fn read(&self) -> Result<Vec<u8>, CredentialReadError>;
    fn write(&self, secret: &[u8]) -> Result<(), String>;
}

fn load_with_store(store: &impl CredentialStore) -> Result<Identity, String> {
    match store.read() {
        Ok(secret) => decode(&Zeroizing::new(secret)),
        Err(CredentialReadError::Missing) => {
            let identity = generate()?;
            let secret = encode(&identity)?;
            store.write(&secret)?;
            Ok(identity)
        }
        Err(CredentialReadError::Unavailable(error)) => Err(error),
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct SystemStore(keyring::Entry);

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl CredentialStore for SystemStore {
    fn read(&self) -> Result<Vec<u8>, CredentialReadError> {
        match self.0.get_secret() {
            Ok(secret) => Ok(secret),
            Err(keyring::Error::NoEntry) => Err(CredentialReadError::Missing),
            Err(_) => Err(CredentialReadError::Unavailable(format!(
                "无法读取 {} 系统身份凭据；请检查系统凭据服务及访问权限，已有身份未被覆盖。",
                storage_name()
            ))),
        }
    }

    fn write(&self, secret: &[u8]) -> Result<(), String> {
        self.0.set_secret(secret).map_err(|_| {
            format!(
                "无法把身份保存到 {}；未写入明文私钥，也不会用临时身份启动服务。",
                storage_name()
            )
        })
    }
}

fn identity_lock(config_dir: &std::path::Path) -> Result<std::fs::File, String> {
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config_dir.join("identity.lock"))
        .map_err(|error| format!("无法打开身份初始化锁：{error}"))?;
    file.lock()
        .map_err(|error| format!("无法锁定身份初始化：{error}"))?;
    Ok(file)
}

pub fn load_or_create(config_dir: &std::path::Path) -> Result<Identity, String> {
    // Serialize first-use reads and writes across application processes.
    let _lock = identity_lock(config_dir)?;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_ACCOUNT)
            .map_err(|_| "无法初始化系统凭据访问，未生成或覆盖身份。")?;
        load_with_store(&SystemStore(entry))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err("当前平台未接入系统凭据存储；仅 Windows 和 macOS 已实现适配。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct MemoryStore {
        secret: RefCell<Option<Vec<u8>>>,
        fail_read: bool,
        fail_write: bool,
        writes: Cell<usize>,
    }

    impl CredentialStore for MemoryStore {
        fn read(&self) -> Result<Vec<u8>, CredentialReadError> {
            if self.fail_read {
                Err(CredentialReadError::Unavailable("凭据服务不可用".into()))
            } else {
                self.secret
                    .borrow()
                    .clone()
                    .ok_or(CredentialReadError::Missing)
            }
        }

        fn write(&self, secret: &[u8]) -> Result<(), String> {
            if self.fail_write {
                return Err("凭据写入被拒绝".into());
            }
            self.writes.set(self.writes.get() + 1);
            *self.secret.borrow_mut() = Some(secret.to_vec());
            Ok(())
        }
    }

    fn store() -> MemoryStore {
        MemoryStore {
            secret: RefCell::new(None),
            fail_read: false,
            fail_write: false,
            writes: Cell::new(0),
        }
    }

    #[test]
    fn initialization_lock_is_exclusive_and_released_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let first = identity_lock(directory.path()).unwrap();
        let second = std::fs::File::options()
            .read(true)
            .write(true)
            .open(directory.path().join("identity.lock"))
            .unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
    }

    #[test]
    fn fingerprint_is_complete_sha256_with_colons() {
        assert_eq!(
            fingerprint(b""),
            "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55"
        );
        let identity = generate().unwrap();
        assert_eq!(identity.fingerprint.len(), 95);
        assert_eq!(
            identity.fingerprint,
            fingerprint(identity.certificate.as_ref())
        );
        assert_eq!(identity.id().len(), 72);
    }

    #[test]
    fn credential_roundtrip_preserves_certificate_key_and_creation_time() {
        let identity = generate().unwrap();
        let secret = encode(&identity).unwrap();
        // Windows generic credentials allow at most 2560 bytes of credential blob.
        assert!(secret.len() < 2560);
        let restored = decode(&secret).unwrap();
        assert_eq!(restored.fingerprint, identity.fingerprint);
        assert_eq!(restored.created_at, identity.created_at);
        assert!(
            restored.private_key.secret_der() == identity.private_key.secret_der(),
            "恢复后的私钥不一致"
        );
    }

    #[test]
    fn malformed_or_mismatched_identity_is_rejected() {
        assert!(decode(b"not json").is_err());
        let first = generate().unwrap();
        let other = generate().unwrap();
        let mut stored: StoredIdentity = serde_json::from_slice(&encode(&first).unwrap()).unwrap();
        stored.created_at = "not a timestamp".into();
        assert!(decode(&serde_json::to_vec(&stored).unwrap()).is_err());
        stored.created_at = first.created_at.clone();
        stored.private_key_der = STANDARD.encode(other.private_key.secret_der());
        assert!(decode(&serde_json::to_vec(&stored).unwrap()).is_err());
        stored.certificate_der = STANDARD.encode(b"not a certificate");
        assert!(decode(&serde_json::to_vec(&stored).unwrap()).is_err());
    }

    #[test]
    fn only_missing_credentials_create_an_identity() {
        let store = store();
        let first = load_with_store(&store).unwrap();
        let second = load_with_store(&store).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(store.writes.get(), 1);
        *store.secret.borrow_mut() = Some(b"corrupted".to_vec());
        assert!(load_with_store(&store).is_err());
        assert_eq!(store.writes.get(), 1);
        assert_eq!(
            store.secret.borrow().as_deref(),
            Some(b"corrupted".as_slice())
        );
    }

    #[test]
    fn credential_access_errors_do_not_fallback_or_overwrite() {
        let mut store = store();
        store.fail_read = true;
        assert!(load_with_store(&store).is_err());
        assert_eq!(store.writes.get(), 0);
        store.fail_read = false;
        store.fail_write = true;
        assert!(load_with_store(&store).is_err());
        assert!(store.secret.borrow().is_none());
    }
}
