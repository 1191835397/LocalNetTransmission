use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, Metadata},
    path::{Component, Path},
};

const MAX_FILES: usize = 128;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub modified_at: Option<String>,
}

fn is_reparse_point(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn reject_link(path: &Path, metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || is_reparse_point(metadata) {
        return Err(format!(
            "不支持符号链接或 Windows 重解析点（包括路径中的链接目录）：{}",
            path.display()
        ));
    }
    Ok(())
}

pub fn inspect(paths: Vec<String>) -> Result<Vec<FileInfo>, String> {
    if paths.len() > MAX_FILES {
        return Err(format!("一次最多检查 {MAX_FILES} 个普通文件，请分批选择。"));
    }
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(paths.len());
    for input in paths {
        let path = Path::new(&input);
        if !path.is_absolute() {
            return Err(format!("文件路径必须是绝对路径：{input}"));
        }
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(format!("文件路径不能包含上级目录跳转：{input}"));
        }
        // Canonicalization would hide links, so inspect ancestors first.
        for ancestor in path.ancestors() {
            let metadata = fs::symlink_metadata(ancestor)
                .map_err(|error| format!("无法检查文件路径「{}」：{error}", ancestor.display()))?;
            reject_link(ancestor, &metadata)?;
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("无法获取文件信息「{input}」：{error}"))?;
        reject_link(path, &metadata)?;
        if metadata.is_dir() {
            return Err(format!("当前仅支持普通文件，不支持文件夹：{input}"));
        }
        if !metadata.is_file() {
            return Err(format!("不是受支持的普通文件：{input}"));
        }
        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("无法解析文件路径「{input}」：{error}"))?;
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("文件名包含无法表示的字符：{input}"))?
            .to_owned();
        let canonical_path = canonical
            .to_str()
            .ok_or_else(|| format!("文件路径包含无法表示的字符：{input}"))?
            .to_owned();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| {
                let seconds = i64::try_from(duration.as_secs()).ok()?;
                DateTime::<Utc>::from_timestamp(seconds, duration.subsec_nanos())
            })
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true));
        result.push(FileInfo {
            path: canonical_path,
            name,
            size: metadata.len(),
            modified_at,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn inspects_metadata_and_deduplicates_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("测试 file.bin");
        fs::write(&path, [0_u8, 1, 2, 3]).unwrap();
        let result = inspect(vec![
            string(&path),
            string(&path),
            string(&root.join(".").join("测试 file.bin")),
        ])
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "测试 file.bin");
        assert_eq!(result[0].size, 4);
        assert!(result[0].modified_at.is_some());
        assert!(inspect(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn rejects_directories_missing_relative_and_excess_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(inspect(vec![string(&root)]).unwrap_err().contains("文件夹"));
        assert!(inspect(vec![string(&root.join("missing"))]).is_err());
        assert!(inspect(vec!["relative.txt".into()]).is_err());
        assert!(inspect(vec![string(&root); 129])
            .unwrap_err()
            .contains("128"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_files_and_ancestor_links() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let file = root.join("file");
        fs::write(&file, b"test").unwrap();
        let link = root.join("link");
        symlink(&file, &link).unwrap();
        assert!(inspect(vec![string(&link)])
            .unwrap_err()
            .contains("符号链接"));
        let linked_directory = root.join("linked-directory");
        symlink(&root, &linked_directory).unwrap();
        assert!(inspect(vec![string(&linked_directory.join("file"))]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_directory_junction_without_symlink_privilege() {
        use std::os::windows::process::CommandExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file"), b"test").unwrap();
        let junction = root.join("junction");
        // Junction creation does not require developer mode or symlink elevation.
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .creation_flags(0x08000000)
            .status()
            .unwrap();
        assert!(status.success(), "无法建立重解析点测试夹具；测试不得假通过");
        let direct = inspect(vec![string(&junction)]);
        let nested = inspect(vec![string(&junction.join("file"))]);
        fs::remove_dir(&junction).unwrap();
        assert!(direct.unwrap_err().contains("重解析点"));
        assert!(nested.unwrap_err().contains("重解析点"));
    }
}
