use std::{
    fs::File,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ATTEMPTS: usize = 1000;

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 180
        || matches!(name, "." | "..")
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return Err("文件名不受支持：必须是最多 180 UTF-8 字节的单个普通文件名。".into());
    }
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = base.to_uppercase();
    let device = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if device {
        return Err("文件名不能使用 Windows 保留设备名。".into());
    }
    Ok(())
}

fn validate_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("文件路径必须是绝对路径，且不能包含上级目录跳转。".into());
    }
    Ok(())
}

fn candidate(name: &str, attempt: usize) -> String {
    if attempt == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({attempt}).{extension}"),
        _ => format!("{name} ({attempt})"),
    }
}

fn temporary_name() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // Exclusivity and access controls, not the name, protect temporary contents.
    format!(".lanlink-{}-{time:x}-{sequence:x}.part", std::process::id())
}

fn error(context: &str, error: std::io::Error) -> String {
    format!("{context}：{error}")
}

pub fn open_source(path: &Path) -> Result<File, String> {
    validate_path(path)?;
    platform::open_source(path)
}

pub struct Destination {
    inner: platform::Destination,
    directory: PathBuf,
    name: String,
    #[cfg(test)]
    fail_after_publish: bool,
}

impl Destination {
    pub fn new(directory: &Path, name: &str) -> Result<Self, String> {
        validate_name(name)?;
        validate_path(directory)?;
        Ok(Self {
            inner: platform::Destination::new(directory)?,
            directory: directory.to_owned(),
            name: name.to_owned(),
            #[cfg(test)]
            fail_after_publish: false,
        })
    }

    pub fn file_mut(&mut self) -> &mut File {
        self.inner.file_mut()
    }

    pub fn check_capacity(&self, size: u64) -> Result<(), String> {
        if self.inner.available_space()? < size {
            return Err("destination_space_insufficient".into());
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<PathBuf, String> {
        self.inner.validate_directory()?;
        self.inner.sync_file()?;
        for attempt in 0..MAX_ATTEMPTS {
            let name = candidate(&self.name, attempt);
            if self.inner.publish(&name)? {
                #[cfg(test)]
                if self.fail_after_publish {
                    return Err("模拟发布后的持久化失败；不得报告传输完成。".into());
                }
                self.inner.finish()?;
                self.inner.validate_directory()?;
                return Ok(self.directory.join(name));
            }
        }
        Err("目标文件名冲突：1000 次不覆盖提交均失败。".into())
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        io,
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
    };

    const DIRECTORY_FLAGS: i32 =
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY;

    fn c_name(name: &std::ffi::OsStr) -> Result<CString, String> {
        CString::new(name.as_bytes()).map_err(|_| "路径不能包含 NUL 字符。".into())
    }

    fn file_from_fd(fd: RawFd) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // Each successful open transfers exactly one descriptor into File.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn stat(file: &File) -> io::Result<libc::stat> {
        let mut value = MaybeUninit::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { value.assume_init() })
    }

    fn same_entry(parent: &File, name: &CStr, file: &File) -> io::Result<()> {
        let owned = stat(file)?;
        let mut entry = MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                entry.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let entry = unsafe { entry.assume_init() };
        if entry.st_dev != owned.st_dev
            || entry.st_ino != owned.st_ino
            || entry.st_mode & libc::S_IFMT != owned.st_mode & libc::S_IFMT
        {
            return Err(io::Error::other("已固定的目录或临时文件被替换"));
        }
        Ok(())
    }

    struct Directory {
        handles: Vec<File>,
        names: Vec<CString>,
    }

    impl Directory {
        fn open(path: &Path) -> Result<Self, String> {
            validate_path(path)?;
            let root = file_from_fd(unsafe { libc::open(c"/".as_ptr(), DIRECTORY_FLAGS) })
                .map_err(|e| error("无法固定文件系统根目录", e))?;
            let mut directory = Self {
                handles: vec![root],
                names: Vec::new(),
            };
            for component in path.components() {
                match component {
                    Component::RootDir | Component::CurDir => (),
                    Component::Normal(name) => {
                        let name = c_name(name)?;
                        let next = file_from_fd(unsafe {
                            libc::openat(
                                directory.file().as_raw_fd(),
                                name.as_ptr(),
                                DIRECTORY_FLAGS,
                            )
                        })
                        .map_err(|e| error("无法安全打开目录；符号链接和特殊路径不受支持", e))?;
                        directory.handles.push(next);
                        directory.names.push(name);
                    }
                    _ => return Err("不受支持的绝对目录路径。".into()),
                }
            }
            Ok(directory)
        }

        fn file(&self) -> &File {
            self.handles.last().expect("directory has a root handle")
        }

        fn validate(&self) -> Result<(), String> {
            // These checks detect relocation; all IO still uses the original descriptors.
            for (index, name) in self.names.iter().enumerate() {
                same_entry(&self.handles[index], name, &self.handles[index + 1])
                    .map_err(|e| error("目录在传输期间被移动或替换", e))?;
            }
            Ok(())
        }
    }

    fn supported_filesystem(directory: &File) -> Result<(), String> {
        let mut info = MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(directory.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(error(
                "无法确认本地文件系统能力",
                io::Error::last_os_error(),
            ));
        }
        let info = unsafe { info.assume_init() };
        #[cfg(target_os = "linux")]
        let supported = matches!(
            info.f_type as u64,
            0xef53 | 0x58465342 | 0x9123683e | 0x01021994 | 0x794c7630
        );
        #[cfg(target_os = "macos")]
        let supported = {
            let name = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }.to_bytes();
            info.f_flags & libc::MNT_LOCAL != 0 && matches!(name, b"apfs" | b"hfs")
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let supported = {
            let _ = info;
            false
        };
        if !supported {
            return Err(
                "此文件系统尚无安全原子提交与持久化保证；不支持网络或用户态文件系统。".into(),
            );
        }
        directory
            .sync_all()
            .map_err(|e| error("目标文件系统不支持目录持久化", e))
    }

    pub(super) fn open_source(path: &Path) -> Result<File, String> {
        let name = path.file_name().ok_or("源路径不是普通文件。")?;
        let parent = path.parent().ok_or("源路径缺少父目录。")?;
        let directory = Directory::open(parent)?;
        let name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                directory.file().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_NOCTTY,
            )
        };
        let file = file_from_fd(fd).map_err(|e| error("无法安全打开源文件；不支持符号链接", e))?;
        let metadata = stat(&file).map_err(|e| error("无法检查源文件句柄", e))?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err("源文件必须是普通文件，不能是目录、设备或管道。".into());
        }
        directory.validate()?;
        Ok(file)
    }

    pub(super) struct Destination {
        file: Option<File>,
        staging: File,
        staging_name: CString,
        directory: Directory,
        temporary_link: bool,
        staging_link: bool,
    }

    impl Destination {
        pub(super) fn new(path: &Path) -> Result<Self, String> {
            let directory = Directory::open(path)?;
            supported_filesystem(directory.file())?;
            let metadata = stat(directory.file()).map_err(|e| error("无法检查接收目录权限", e))?;
            // Sticky directories also prevent other users from replacing our staging entry.
            if metadata.st_mode & 0o022 != 0 && metadata.st_mode & libc::S_ISVTX == 0 {
                return Err(
                    "接收目录允许其他用户替换临时条目；请选择私有目录或 sticky 目录。".into(),
                );
            }
            for _ in 0..MAX_ATTEMPTS {
                let staging_name = CString::new(temporary_name()).expect("temporary name is ASCII");
                if unsafe {
                    libc::mkdirat(directory.file().as_raw_fd(), staging_name.as_ptr(), 0o700)
                } != 0
                {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EEXIST) {
                        continue;
                    }
                    return Err(error("无法创建私有临时目录", e));
                }
                let staging = file_from_fd(unsafe {
                    libc::openat(
                        directory.file().as_raw_fd(),
                        staging_name.as_ptr(),
                        DIRECTORY_FLAGS,
                    )
                })
                .map_err(|e| error("无法固定私有临时目录", e))?;
                let metadata = stat(&staging).map_err(|e| error("无法检查临时目录权限", e))?;
                if metadata.st_uid != unsafe { libc::geteuid() } || metadata.st_mode & 0o077 != 0 {
                    return Err("临时目录不属于当前用户或不具备私有权限。".into());
                }
                let mut destination = Self {
                    file: None,
                    staging,
                    staging_name,
                    directory,
                    temporary_link: false,
                    staging_link: true,
                };
                let file = file_from_fd(unsafe {
                    libc::openat(
                        destination.staging.as_raw_fd(),
                        c"payload".as_ptr(),
                        libc::O_RDWR
                            | libc::O_CLOEXEC
                            | libc::O_NOFOLLOW
                            | libc::O_CREAT
                            | libc::O_EXCL,
                        0o600,
                    )
                })
                .map_err(|e| error("无法独占创建临时文件", e))?;
                destination.file = Some(file);
                destination.temporary_link = true;
                destination.validate_directory()?;
                return Ok(destination);
            }
            Err("无法分配独占临时目录。".into())
        }

        pub(super) fn file_mut(&mut self) -> &mut File {
            self.file.as_mut().expect("initialized temporary file")
        }

        pub(super) fn validate_directory(&self) -> Result<(), String> {
            self.directory.validate()
        }

        pub(super) fn available_space(&self) -> Result<u64, String> {
            let mut value = MaybeUninit::<libc::statvfs>::uninit();
            if unsafe { libc::fstatvfs(self.directory.file().as_raw_fd(), value.as_mut_ptr()) } != 0
            {
                return Err("destination_space_unavailable".into());
            }
            let value = unsafe { value.assume_init() };
            Ok((value.f_bavail as u64).saturating_mul(value.f_frsize as u64))
        }

        pub(super) fn sync_file(&mut self) -> Result<(), String> {
            self.file_mut()
                .sync_all()
                .map_err(|e| error("临时文件持久化失败", e))?;
            #[cfg(target_os = "macos")]
            if unsafe { libc::fcntl(self.file_mut().as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
                return Err(error(
                    "文件系统不支持完整持久化屏障",
                    io::Error::last_os_error(),
                ));
            }
            Ok(())
        }

        pub(super) fn publish(&mut self, name: &str) -> Result<bool, String> {
            let file = self.file.as_ref().expect("initialized temporary file");
            same_entry(&self.staging, c"payload", file)
                .map_err(|e| error("临时文件所有权已改变", e))?;
            let name = CString::new(name).expect("validated basename");
            // The source name lives inside a pinned 0700 directory, never the shared target namespace.
            if unsafe {
                libc::linkat(
                    self.staging.as_raw_fd(),
                    c"payload".as_ptr(),
                    self.directory.file().as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            } == 0
            {
                return Ok(true);
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EEXIST) {
                return Ok(false);
            }
            Err(error("文件系统拒绝不覆盖的原子发布；未使用不安全回退", e))
        }

        fn cleanup(&mut self) -> io::Result<()> {
            if self.temporary_link {
                same_entry(
                    &self.staging,
                    c"payload",
                    self.file.as_ref().expect("owned temporary file"),
                )?;
                if unsafe { libc::unlinkat(self.staging.as_raw_fd(), c"payload".as_ptr(), 0) } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                self.temporary_link = false;
            }
            if self.staging_link {
                same_entry(self.directory.file(), &self.staging_name, &self.staging)?;
                if unsafe {
                    libc::unlinkat(
                        self.directory.file().as_raw_fd(),
                        self.staging_name.as_ptr(),
                        libc::AT_REMOVEDIR,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                self.staging_link = false;
            }
            Ok(())
        }

        pub(super) fn finish(&mut self) -> Result<(), String> {
            self.cleanup()
                .map_err(|e| error("文件已发布，但临时条目清理失败；不能确认完成", e))?;
            self.directory
                .file()
                .sync_all()
                .map_err(|e| error("文件已发布，但目录持久化失败；不能确认完成", e))
        }
    }

    impl Drop for Destination {
        fn drop(&mut self) {
            // Never unlink the published name, including on post-publication failures.
            let _ = self.cleanup();
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        io,
        mem::{offset_of, size_of},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::Prefix,
        ptr::{null, null_mut},
    };
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    };
    use windows_sys::Win32::{
        Foundation::{
            RtlNtStatusToDosError, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ,
            GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        },
        Security::{
            AddAccessAllowedAce, CreateWellKnownSid, InitializeAcl, InitializeSecurityDescriptor,
            SetSecurityDescriptorControl, SetSecurityDescriptorDacl, WinCreatorOwnerRightsSid, ACL,
            ACL_REVISION, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    };

    fn handle(file: &File) -> HANDLE {
        file.as_raw_handle() as HANDLE
    }

    fn wide(path: &Path) -> Result<Vec<u16>, String> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err("路径不能包含 NUL 字符。".into());
        }
        value.push(0);
        Ok(value)
    }

    fn open(
        path: &Path,
        access: u32,
        share: u32,
        disposition: u32,
        flags: u32,
        security: *const SECURITY_ATTRIBUTES,
    ) -> Result<File, io::Error> {
        let name = wide(path).map_err(io::Error::other)?;
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(raw) })
    }

    fn check_file(file: &File, directory: bool) -> Result<(), String> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        if unsafe {
            GetFileInformationByHandleEx(
                handle(file),
                FileAttributeTagInfo,
                (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } == 0
        {
            return Err(error("无法检查文件句柄属性", io::Error::last_os_error()));
        }
        if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("不支持 Windows 重解析点（包括符号链接、junction 和挂载点）。".into());
        }
        if (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
            || unsafe { GetFileType(handle(file)) } != FILE_TYPE_DISK
        {
            return Err("文件句柄类型不受支持；仅支持普通文件与普通目录。".into());
        }
        Ok(())
    }

    struct Directory {
        handles: Vec<File>,
        path: PathBuf,
    }

    impl Directory {
        fn open(path: &Path) -> Result<Self, String> {
            validate_path(path)?;
            let mut components = path.components();
            let drive = match components.next() {
                Some(Component::Prefix(prefix)) => match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)
                        if drive.is_ascii_alphabetic() =>
                    {
                        drive
                    }
                    _ => return Err("仅支持本地磁盘绝对路径；不支持 UNC 或设备路径。".into()),
                },
                _ => return Err("目录缺少本地磁盘前缀。".into()),
            };
            if components.next() != Some(Component::RootDir) {
                return Err("必须提供磁盘根目录起始的绝对路径。".into());
            }
            let root_path = PathBuf::from(format!("\\\\?\\{}:\\", char::from(drive)));
            let root = Self::open_component(&root_path)?;
            let mut filesystem = [0_u16; 32];
            let mut flags = 0;
            if unsafe {
                GetVolumeInformationByHandleW(
                    handle(&root),
                    null_mut(),
                    0,
                    null_mut(),
                    null_mut(),
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } == 0
            {
                return Err(error(
                    "无法确认本地文件系统能力",
                    io::Error::last_os_error(),
                ));
            }
            let length = filesystem
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(filesystem.len());
            if String::from_utf16_lossy(&filesystem[..length]) != "NTFS" || flags & 0x8 == 0 {
                return Err("P2 安全落盘仅支持具有持久 ACL 的本地 NTFS 卷。".into());
            }
            let mut volume_path = vec![0_u16; 32768];
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    handle(&root),
                    volume_path.as_mut_ptr(),
                    volume_path.len() as u32,
                    VOLUME_NAME_GUID,
                )
            } as usize;
            if length == 0 || length >= volume_path.len() {
                return Err(error("无法固定本地卷路径", io::Error::last_os_error()));
            }
            let volume_path =
                String::from_utf16(&volume_path[..length]).map_err(|_| "本地卷路径不受支持。")?;
            if !volume_path.starts_with("\\\\?\\Volume{") || !volume_path.ends_with("}\\") {
                return Err("不支持网络映射或替代磁盘根路径。".into());
            }
            let mut directory = Self {
                handles: vec![root],
                path: PathBuf::from(volume_path),
            };
            for component in components {
                match component {
                    Component::CurDir => (),
                    Component::Normal(name) => {
                        validate_name(name.to_str().ok_or("目录名包含不受支持的 Unicode。")?)?;
                        directory.path.push(name);
                        directory
                            .handles
                            .push(Self::open_component(&directory.path)?);
                    }
                    _ => return Err("不受支持的目录路径组件。".into()),
                }
            }
            Ok(directory)
        }

        fn open_component(path: &Path) -> Result<File, String> {
            // Denying WRITE blocks reparse mutation; denying DELETE pins this ancestor's name.
            let file = open(
                path,
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE,
                FILE_SHARE_READ,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null(),
            )
            .map_err(|e| error("无法固定目录（拒绝共享写入、删除或重解析）", e))?;
            check_file(&file, true)?;
            Ok(file)
        }
    }

    pub(super) fn open_source(path: &Path) -> Result<File, String> {
        let name = path.file_name().ok_or("源路径不是普通文件。")?;
        validate_name(name.to_str().ok_or("源文件名包含不受支持的 Unicode。")?)?;
        let directory = Directory::open(path.parent().ok_or("源路径缺少父目录。")?)?;
        let file = open(
            &directory.path.join(name),
            GENERIC_READ,
            FILE_SHARE_READ,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null(),
        )
        .map_err(|e| error("无法安全打开源文件", e))?;
        check_file(&file, false)?;
        Ok(file)
    }

    fn private_file(path: &Path) -> io::Result<File> {
        let mut sid = [0_u32; 17];
        let mut sid_size = size_of_val(&sid) as u32;
        let mut acl = [0_u32; 64];
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast::<c_void>();
        // OWNER RIGHTS permits only the assigned owner and disables inherited directory grants.
        let ok = unsafe {
            CreateWellKnownSid(
                WinCreatorOwnerRightsSid,
                null_mut(),
                sid.as_mut_ptr().cast(),
                &mut sid_size,
            ) != 0
                && InitializeAcl(acl_ptr, size_of_val(&acl) as u32, ACL_REVISION) != 0
                && AddAccessAllowedAce(
                    acl_ptr,
                    ACL_REVISION,
                    FILE_ALL_ACCESS,
                    sid.as_mut_ptr().cast(),
                ) != 0
                && InitializeSecurityDescriptor(descriptor_ptr, 1) != 0
                && SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) != 0
                && SetSecurityDescriptorControl(
                    descriptor_ptr,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                ) != 0
        };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        open(
            path,
            GENERIC_READ | GENERIC_WRITE | DELETE,
            0,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            &security,
        )
    }

    pub(super) struct Destination {
        file: File,
        directory: Directory,
        published: bool,
    }

    impl Destination {
        pub(super) fn new(path: &Path) -> Result<Self, String> {
            let directory = Directory::open(path)?;
            for _ in 0..MAX_ATTEMPTS {
                match private_file(&directory.path.join(temporary_name())) {
                    Ok(file) => {
                        let destination = Self {
                            file,
                            directory,
                            published: false,
                        };
                        check_file(&destination.file, false)?;
                        return Ok(destination);
                    }
                    Err(e) if matches!(e.raw_os_error(), Some(code) if code == ERROR_FILE_EXISTS as i32 || code == ERROR_ALREADY_EXISTS as i32) =>
                        {}
                    Err(e) => return Err(error("无法独占创建私有临时文件", e)),
                }
            }
            Err("无法分配独占临时文件。".into())
        }

        pub(super) fn file_mut(&mut self) -> &mut File {
            &mut self.file
        }

        pub(super) fn validate_directory(&self) -> Result<(), String> {
            // Ancestor handles remain locked against rename, deletion and reparse mutation.
            for directory in &self.directory.handles {
                check_file(directory, true)?;
            }
            Ok(())
        }

        pub(super) fn available_space(&self) -> Result<u64, String> {
            let path = wide(&self.directory.path)?;
            let mut available = 0u64;
            if unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut available, null_mut(), null_mut()) }
                == 0
            {
                return Err("destination_space_unavailable".into());
            }
            Ok(available)
        }

        pub(super) fn sync_file(&mut self) -> Result<(), String> {
            self.file
                .sync_all()
                .map_err(|e| error("临时文件持久化失败", e))
        }

        pub(super) fn publish(&mut self, name: &str) -> Result<bool, String> {
            // A native basename-only rename stays in the source handle's directory without reopening it.
            let name: Vec<u16> = name.encode_utf16().collect();
            let bytes = (offset_of!(FILE_RENAME_INFORMATION, FileName)
                + name.len() * size_of::<u16>())
            .max(size_of::<FILE_RENAME_INFORMATION>());
            let allocation = bytes.div_ceil(size_of::<usize>());
            let mut buffer = vec![0_usize; allocation];
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
            let mut result = IO_STATUS_BLOCK::default();
            let status = unsafe {
                (*info).Anonymous.ReplaceIfExists = false;
                (*info).RootDirectory = null_mut();
                (*info).FileNameLength = (name.len() * size_of::<u16>()) as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    (*info).FileName.as_mut_ptr(),
                    name.len(),
                );
                NtSetInformationFile(
                    handle(&self.file),
                    &mut result,
                    info.cast(),
                    bytes as u32,
                    FileRenameInformation,
                )
            };
            if status >= 0 {
                self.published = true;
                return Ok(true);
            }
            let e = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
            if matches!(e.raw_os_error(), Some(code) if code == ERROR_FILE_EXISTS as i32 || code == ERROR_ALREADY_EXISTS as i32)
            {
                return Ok(false);
            }
            Err(error("文件系统拒绝基于句柄的不覆盖发布；未使用路径回退", e))
        }

        pub(super) fn finish(&mut self) -> Result<(), String> {
            // NTFS flushes file metadata as well as data; the handle also uses WRITE_THROUGH.
            self.file
                .sync_all()
                .map_err(|e| error("文件已发布，但持久化失败；不能确认完成", e))
        }
    }

    impl Drop for Destination {
        fn drop(&mut self) {
            if !self.published {
                let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
                // Delete the owned handle, never whatever now occupies a pathname.
                unsafe {
                    SetFileInformationByHandle(
                        handle(&self.file),
                        FileDispositionInfo,
                        (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                        size_of::<FILE_DISPOSITION_INFO>() as u32,
                    );
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub(super) fn open_source(_: &Path) -> Result<File, String> {
        Err("此平台没有安全文件 IO 实现。".into())
    }
    pub(super) struct Destination;
    impl Destination {
        pub(super) fn new(_: &Path) -> Result<Self, String> {
            Err("此平台没有安全落盘实现。".into())
        }
        pub(super) fn file_mut(&mut self) -> &mut File {
            unreachable!("unsupported platform cannot construct a destination")
        }
        pub(super) fn validate_directory(&self) -> Result<(), String> {
            Err("不支持此平台。".into())
        }
        pub(super) fn available_space(&self) -> Result<u64, String> {
            Err("destination_space_unavailable".into())
        }
        pub(super) fn sync_file(&mut self) -> Result<(), String> {
            Err("不支持此平台。".into())
        }
        pub(super) fn publish(&mut self, _: &str) -> Result<bool, String> {
            Err("不支持此平台。".into())
        }
        pub(super) fn finish(&mut self) -> Result<(), String> {
            Err("不支持此平台。".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
    };

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        // Test fixtures may start below macOS /var or Windows short-name aliases.
        let root = directory.path().canonicalize().unwrap();
        (directory, root)
    }

    #[test]
    fn validates_cross_platform_basenames_without_unicode_transformation() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:a",
            "file:stream",
            "a\0b",
            "a\nb",
            "a\u{85}b",
            "a.",
            "a ",
            "CON",
            "con.txt",
            "AUX.tar.gz",
            "PRN",
            "nul.exe",
            "COM1.log",
            "com9",
            "LPT1.txt",
            "lpt9",
            "COM¹.txt",
            "COM²",
            "LPT³.dat",
            "CON .txt",
            "CONIN$",
            "CONOUT$.log",
            "a?",
            "a*",
            "a|",
            "a<",
            "a>",
            "a\"",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_name(&"x".repeat(181)).is_err());
        assert!(validate_name(&"测".repeat(61)).is_err());
        for name in [
            "普通文件.txt",
            "a b.txt",
            ".hidden",
            "COM10.txt",
            "LPT0",
            "é.txt",
            "e\u{301}.txt",
        ] {
            validate_name(name).unwrap();
        }
        validate_name(&"x".repeat(180)).unwrap();
    }

    #[test]
    fn commits_normal_and_empty_files_and_source_handle_supports_seek() {
        let (_fixture, root) = fixture();
        for (name, bytes) in [
            ("normal.txt", b"hello\0world".as_slice()),
            ("empty", b"".as_slice()),
        ] {
            let mut destination = Destination::new(&root, name).unwrap();
            destination.file_mut().write_all(bytes).unwrap();
            let path = destination.commit().unwrap();
            assert_eq!(path, root.join(name));
            let mut source = open_source(&path).unwrap();
            let mut received = Vec::new();
            source.read_to_end(&mut received).unwrap();
            assert_eq!(received, bytes);
            source.seek(SeekFrom::Start(0)).unwrap();
            received.clear();
            source.read_to_end(&mut received).unwrap();
            assert_eq!(received, bytes);
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
    }

    #[test]
    fn capacity_checks_reject_impossible_size_and_clean_temporary_files() {
        let (_fixture, root) = fixture();
        let destination = Destination::new(&root, "capacity.bin").unwrap();
        destination.check_capacity(0).unwrap();
        assert_eq!(
            destination.check_capacity(u64::MAX).unwrap_err(),
            "destination_space_insufficient"
        );
        drop(destination);
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn repeated_names_and_directories_are_never_overwritten() {
        let (_fixture, root) = fixture();
        fs::write(root.join("file.txt"), b"original").unwrap();
        fs::create_dir(root.join("file (1).txt")).unwrap();
        for expected in ["file (2).txt", "file (3).txt"] {
            let mut destination = Destination::new(&root, "file.txt").unwrap();
            destination.file_mut().write_all(b"new").unwrap();
            assert_eq!(destination.commit().unwrap(), root.join(expected));
        }
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"original");
        assert!(root.join("file (1).txt").is_dir());
    }

    #[test]
    fn cancellation_and_exhausted_commit_remove_only_owned_temporary_entries() {
        let (_fixture, root) = fixture();
        let destination = Destination::new(&root, "cancel").unwrap();
        assert!(!root.join("cancel").exists());
        drop(destination);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        for attempt in 0..MAX_ATTEMPTS {
            fs::write(root.join(candidate("full.txt", attempt)), b"untouched").unwrap();
        }
        let mut destination = Destination::new(&root, "full.txt").unwrap();
        destination
            .file_mut()
            .write_all(b"must not replace")
            .unwrap();
        assert!(destination.commit().is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_ATTEMPTS);
        for attempt in 0..MAX_ATTEMPTS {
            assert_eq!(
                fs::read(root.join(candidate("full.txt", attempt))).unwrap(),
                b"untouched"
            );
        }
    }

    #[test]
    fn failure_after_publication_is_not_success_and_does_not_delete_output() {
        let (_fixture, root) = fixture();
        let mut destination = Destination::new(&root, "published").unwrap();
        destination.file_mut().write_all(b"keep").unwrap();
        destination.fail_after_publish = true;
        assert!(destination.commit().is_err());
        assert_eq!(fs::read(root.join("published")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn rejects_nonfile_source_relative_traversal_and_invalid_destination_before_creation() {
        let (_fixture, root) = fixture();
        assert!(open_source(&root).is_err());
        assert!(open_source(&root.join("missing")).is_err());
        assert!(open_source(Path::new("relative")).is_err());
        // PathBuf::push normalizes parent components in Windows verbatim paths.
        let mut traversal = root.as_os_str().to_owned();
        traversal.push(format!("{}..", std::path::MAIN_SEPARATOR));
        let traversal = PathBuf::from(traversal);
        assert!(open_source(&traversal).is_err());
        assert!(Destination::new(Path::new("relative"), "file").is_err());
        assert!(Destination::new(&traversal, "file").is_err());
        assert!(Destination::new(&root, "../escape").is_err());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unix_links_and_pipes_are_rejected_and_colliding_links_are_untouched() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };
        let (_fixture, root) = fixture();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("source"), b"original").unwrap();
        symlink(&target, root.join("linked-directory")).unwrap();
        symlink(target.join("source"), root.join("linked-file")).unwrap();
        symlink(root.join("absent"), root.join("dangling")).unwrap();
        assert!(open_source(&root.join("linked-file")).is_err());
        assert!(open_source(&root.join("linked-directory/source")).is_err());
        assert!(Destination::new(&root.join("linked-directory"), "file").is_err());
        let fifo = root.join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_source(&fifo).is_err());
        for name in ["linked-file", "linked-directory", "dangling"] {
            let destination = Destination::new(&root, name).unwrap();
            assert_eq!(
                destination.commit().unwrap(),
                root.join(format!("{name} (1)"))
            );
            assert!(fs::symlink_metadata(root.join(name))
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert_eq!(fs::read(target.join("source")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn unix_directory_replacement_cannot_redirect_commit_or_cleanup() {
        use std::os::unix::fs::symlink;
        let (_fixture, root) = fixture();
        let original = root.join("receive");
        let moved = root.join("moved");
        let outside = root.join("outside");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&outside).unwrap();
        let mut destination = Destination::new(&original, "file").unwrap();
        destination.file_mut().write_all(b"private").unwrap();
        fs::rename(&original, &moved).unwrap();
        symlink(&outside, &original).unwrap();
        assert!(destination.commit().is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
        assert!(fs::symlink_metadata(&original)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn unix_source_handle_survives_path_replacement_without_reopening() {
        let (_fixture, root) = fixture();
        let source = root.join("source");
        fs::write(&source, b"original").unwrap();
        let mut file = open_source(&source).unwrap();
        fs::rename(&source, root.join("old")).unwrap();
        fs::write(&source, b"replacement").unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "original");
    }

    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        use std::os::windows::process::CommandExt;
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .creation_flags(0x08000000)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "junction fixture must be created, not silently skipped"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_ancestors_are_rejected_and_collision_is_not_replaced() {
        let (_fixture, root) = fixture();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("source"), b"original").unwrap();
        let link = root.join("junction");
        junction(&link, &target);
        assert!(open_source(&link.join("source")).is_err());
        assert!(open_source(&link).is_err());
        assert!(Destination::new(&link, "file").is_err());
        let destination = Destination::new(&root, "junction").unwrap();
        assert_eq!(destination.commit().unwrap(), root.join("junction (1)"));
        assert_eq!(fs::read(target.join("source")).unwrap(), b"original");
        fs::remove_dir(&link).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_directory_cannot_be_replaced_or_opened_for_reparse_mutation() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_fixture, root) = fixture();
        let ancestor = root.join("ancestor");
        let receive = ancestor.join("receive");
        fs::create_dir_all(&receive).unwrap();
        let destination = Destination::new(&receive, "file").unwrap();
        assert!(fs::rename(&receive, ancestor.join("moved")).is_err());
        assert!(fs::rename(&ancestor, root.join("moved-ancestor")).is_err());
        for path in [&ancestor, &receive] {
            assert!(fs::OpenOptions::new()
                .write(true)
                .share_mode(7)
                .custom_flags(0x02000000 | 0x00200000)
                .open(path)
                .is_err());
        }
        destination.commit().unwrap();
        fs::rename(&ancestor, root.join("moved-ancestor")).unwrap();
    }
}
