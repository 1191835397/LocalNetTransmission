use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const DEFAULT_PORT: u32 = 53318;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub device_name: String,
    pub receive_dir: String,
    pub port: u32,
}

pub fn config_file(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

fn validate_fields(mut settings: Settings) -> Result<Settings, String> {
    settings.device_name = settings.device_name.trim().to_owned();
    if !(1..=64).contains(&settings.device_name.chars().count()) {
        return Err("设备名称去除首尾空白后须为 1–64 个字符。".into());
    }
    if settings.device_name.chars().any(char::is_control) {
        return Err("设备名称不能包含控制字符。".into());
    }
    if !(1024..=65535).contains(&settings.port) {
        return Err("监听端口须在 1024–65535 之间。".into());
    }
    let directory = Path::new(&settings.receive_dir);
    if !directory.is_absolute() {
        return Err("接收目录必须是绝对路径。".into());
    }
    Ok(settings)
}

pub fn validate(settings: Settings) -> Result<Settings, String> {
    let settings = validate_fields(settings)?;
    let directory = Path::new(&settings.receive_dir);
    let metadata = fs::metadata(directory)
        .map_err(|error| format!("无法访问接收目录「{}」：{error}", directory.display()))?;
    if !metadata.is_dir() {
        return Err("接收路径必须是已存在的目录，不能是文件。".into());
    }
    Ok(settings)
}

fn defaults() -> Result<Settings, String> {
    let name = hostname::get().map_err(|error| format!("无法读取本机名称：{error}"))?;
    let directory = dirs::download_dir()
        .filter(|path| path.is_absolute() && path.is_dir())
        .or_else(|| dirs::home_dir().filter(|path| path.is_absolute() && path.is_dir()))
        .ok_or("未找到可用的下载目录或用户主目录。")?;
    let receive_dir = directory
        .to_str()
        .ok_or("默认接收目录包含无法表示的路径字符。")?
        .to_owned();
    validate(Settings {
        device_name: name.to_string_lossy().into_owned(),
        receive_dir,
        port: DEFAULT_PORT,
    })
}

pub fn load_or_create(config_dir: &Path) -> Result<Settings, String> {
    fs::create_dir_all(config_dir)
        .map_err(|error| format!("无法创建配置目录「{}」：{error}", config_dir.display()))?;
    let path = config_file(config_dir);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("配置路径不是普通文件：{}", path.display()));
            }
            read(&path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let settings = defaults()?;
            persist(&path, &settings)?;
            Ok(settings)
        }
        Err(error) => Err(format!("无法检查配置文件「{}」：{error}", path.display())),
    }
}

pub fn read(path: &Path) -> Result<Settings, String> {
    let file = File::open(path)
        .map_err(|error| format!("无法读取配置文件「{}」：{error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取配置文件失败：{error}"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("配置文件超过 64 KiB，拒绝加载；原文件已保留。".into());
    }
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Result<Settings, String> {
    let settings = serde_json::from_slice(bytes)
        .map_err(|error| format!("配置文件损坏或格式不兼容，未恢复默认值：{error}"))?;
    validate_fields(settings).map_err(|error| format!("配置内容无效，原文件已保留：{error}"))
}

pub fn save(config_dir: &Path, settings: Settings) -> Result<Settings, String> {
    let settings = validate(settings)?;
    persist(&config_file(config_dir), &settings)?;
    Ok(settings)
}

fn persist(path: &Path, settings: &Settings) -> Result<(), String> {
    let parent = path.parent().ok_or("配置文件缺少父目录。")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".lanlink-settings-")
        .tempfile_in(parent)
        .map_err(|error| format!("无法在配置目录创建同卷临时文件：{error}"))?;
    serde_json::to_writer_pretty(&mut temporary, settings)
        .map_err(|error| format!("写入临时配置失败：{error}"))?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("同步临时配置失败：{error}"))?;
    // Keep the previous file intact until the same-directory atomic replacement.
    temporary
        .persist(path)
        .map_err(|error| format!("原子替换配置失败，旧配置未主动删除：{}", error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(directory: &Path) -> Settings {
        Settings {
            device_name: "  邻传测试  ".into(),
            receive_dir: directory.to_str().unwrap().into(),
            port: DEFAULT_PORT,
        }
    }

    #[test]
    fn validates_name_by_characters_and_trims() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = valid(directory.path());
        assert_eq!(validate(settings.clone()).unwrap().device_name, "邻传测试");
        for name in [
            String::new(),
            "   ".into(),
            "a\nb".into(),
            "a\0b".into(),
            "邻".repeat(65),
        ] {
            settings.device_name = name;
            assert!(validate(settings.clone()).is_err());
        }
        settings.device_name = "邻".repeat(64);
        assert!(validate(settings).is_ok());
    }

    #[test]
    fn validates_port_edges_and_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = valid(directory.path());
        for port in [0, 1023, 65536, u32::MAX] {
            settings.port = port;
            assert!(validate(settings.clone()).is_err());
        }
        for port in [1024, 65535] {
            settings.port = port;
            assert!(validate(settings.clone()).is_ok());
        }
        settings.receive_dir = "relative".into();
        assert!(validate(settings.clone()).is_err());
        settings.receive_dir = directory.path().join("missing").to_str().unwrap().into();
        assert!(validate(settings.clone()).is_err());
        let file = directory.path().join("file");
        fs::write(&file, b"").unwrap();
        settings.receive_dir = file.to_str().unwrap().into();
        assert!(validate(settings).is_err());
    }

    #[test]
    fn atomically_persists_and_replaces_without_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let first = save(directory.path(), valid(directory.path())).unwrap();
        let path = config_file(directory.path());
        assert_eq!(read(&path).unwrap(), first);
        let mut second = first.clone();
        second.device_name = "第二台设备".into();
        second.port = 53319;
        save(directory.path(), second.clone()).unwrap();
        assert_eq!(read(&path).unwrap(), second);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let mut invalid = second.clone();
        invalid.port = 80;
        assert!(save(directory.path(), invalid).is_err());
        assert_eq!(read(&path).unwrap(), second);
    }

    #[test]
    fn corrupted_configuration_is_not_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(directory.path());
        fs::write(&path, b"{broken").unwrap();
        assert!(load_or_create(directory.path())
            .unwrap_err()
            .contains("损坏"));
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        assert!(parse(br#"{"deviceName":"x","receiveDir":"/","port":-1}"#).is_err());
        assert!(parse(br#"{"device_name":"x","receiveDir":"/","port":53318}"#).is_err());
    }

    #[test]
    fn replacement_failure_preserves_existing_directory_and_cleans_temp_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(config_file(directory.path())).unwrap();
        assert!(save(directory.path(), valid(directory.path())).is_err());
        assert!(config_file(directory.path()).is_dir());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_receive_directory_does_not_block_settings_repair() {
        let directory = tempfile::tempdir().unwrap();
        let receive = directory.path().join("receive");
        fs::create_dir(&receive).unwrap();
        let saved = save(directory.path(), valid(&receive)).unwrap();
        fs::remove_dir(&receive).unwrap();
        assert_eq!(load_or_create(directory.path()).unwrap(), saved);
        assert!(validate(saved).is_err());
        assert!(save(directory.path(), valid(directory.path())).is_ok());
    }

    #[test]
    fn oversized_configuration_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(directory.path());
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(read(&path).unwrap_err().contains("64 KiB"));
    }
}
