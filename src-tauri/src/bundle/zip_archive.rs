//! ZIP32 STORE packaging and hardened extraction for portable session bundles.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use super::{ensure_plain_directory_path, validate_plain_directory_tree};
use crate::atomic_file;
use crate::error::{AppError, AppResult};
use crate::models::ZipReport;
use crate::paths;

static ZIP_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// ========================= zip 打包 =========================

pub fn pack_bundles_zip(src_dir: String, zip_path: String) -> AppResult<ZipReport> {
    let src = absolute_lexical_path(Path::new(&src_dir), "ZIP 打包源")?;
    validate_existing_directory_chain(&src, "ZIP 打包源父链")?;
    validate_plain_directory_tree(&src, "ZIP 打包源")?;

    let out = absolute_lexical_path(Path::new(&zip_path), "ZIP 输出")?;
    let parent = out
        .parent()
        .ok_or_else(|| AppError::Path(format!("ZIP 输出缺少父目录: {}", out.to_string_lossy())))?;
    ensure_plain_directory_path(parent, "ZIP 输出父目录")?;
    validate_existing_directory_chain(parent, "ZIP 输出父目录")?;
    match fs::symlink_metadata(&out) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::path_safety::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(AppError::Path(format!(
                "ZIP 输出已存在但不是普通文件，或属于链接/reparse point: {}",
                out.to_string_lossy()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let src_canon = fs::canonicalize(&src)?;
    let out_parent_canon = fs::canonicalize(parent)?;
    if out_parent_canon == src_canon || out_parent_canon.starts_with(&src_canon) {
        return Err(AppError::Path(format!(
            "zip 输出路径不能位于被打包目录内部: 输出 {}, 源目录 {}",
            out.to_string_lossy(),
            src.to_string_lossy()
        )));
    }

    let (stage_path, stage_file) = create_unique_zip_pack_stage(parent, &out)?;
    let write_result = write_store_zip(&src, stage_file);
    let (file_count, total_bytes) = match write_result {
        Ok(report) => report,
        Err(error) => {
            return Err(match fs::remove_file(&stage_path) {
                Ok(()) => error,
                Err(cleanup_error) => AppError::Other(format!(
                    "{error}; 清理 ZIP 打包暂存文件失败 {}: {cleanup_error}",
                    stage_path.to_string_lossy()
                )),
            })
        }
    };
    if let Err(error) = publish_packed_zip(&stage_path, &out) {
        return Err(match fs::remove_file(&stage_path) {
            Ok(()) => error,
            Err(cleanup_error) => AppError::Other(format!(
                "{error}; 清理 ZIP 打包暂存文件失败 {}: {cleanup_error}",
                stage_path.to_string_lossy()
            )),
        });
    }
    fs::remove_file(&stage_path).map_err(|error| {
        AppError::Other(format!(
            "ZIP 已原子发布，但清理暂存文件失败 {}: {error}",
            stage_path.to_string_lossy()
        ))
    })?;

    Ok(ZipReport {
        path: out.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

fn create_unique_zip_pack_stage(parent: &Path, output: &Path) -> AppResult<(PathBuf, File)> {
    let file_name = output.file_name().ok_or_else(|| {
        AppError::Path(format!("ZIP 输出缺少文件名: {}", output.to_string_lossy()))
    })?;
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut stage_name = file_name.to_os_string();
        stage_name.push(format!(
            ".{}.{}.ccsm-pack.tmp",
            std::process::id(),
            sequence
        ));
        let stage = parent.join(stage_name);
        match OpenOptions::new().write(true).create_new(true).open(&stage) {
            Ok(file) => return Ok((stage, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_store_zip(src: &Path, file: File) -> AppResult<(u32, u64)> {
    let canonical_src = src.canonicalize()?;
    let mut writer = BufWriter::new(file);
    let mut central: Vec<CentralEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut file_count: u32 = 0;
    let mut offset: u32 = 0;

    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Other(format!(
                "遍历待打包 bundle 目录失败 {}: {error}",
                src.to_string_lossy()
            ))
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
            return Err(AppError::Path(format!(
                "ZIP 打包源包含链接/junction/reparse point: {}",
                entry.path().to_string_lossy()
            )));
        }
        if !entry.path().canonicalize()?.starts_with(&canonical_src) {
            return Err(AppError::Path(format!(
                "ZIP 打包源条目解析后逃出根目录: {}",
                entry.path().to_string_lossy()
            )));
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(AppError::Path(format!(
                "ZIP 打包源包含不支持的文件类型: {}",
                entry.path().to_string_lossy()
            )));
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .map(|p| p.to_path_buf())
            .map_err(|error| {
                AppError::Path(format!(
                    "无法计算 zip 内相对路径 {}: {error}",
                    entry.path().to_string_lossy()
                ))
            })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        checked_zip_entry_path(&rel_str, false)?;
        let name_len = u16::try_from(rel_str.len())
            .map_err(|_| AppError::Other(format!("ZIP 文件名过长: {rel_str}")))?;
        let size = u32::try_from(metadata.len()).map_err(|_| {
            AppError::Other(format!(
                "ZIP STORE 不支持超过 4 GiB 的文件: {}",
                entry.path().to_string_lossy()
            ))
        })?;
        let mut data: Vec<u8> = Vec::new();
        File::open(entry.path())?.read_to_end(&mut data)?;
        if data.len() as u64 != metadata.len() {
            return Err(AppError::Other(format!(
                "ZIP 打包源在读取期间发生变化: {}",
                entry.path().to_string_lossy()
            )));
        }
        let crc = crc32(&data);
        total_bytes = total_bytes
            .checked_add(size as u64)
            .ok_or_else(|| AppError::Other("ZIP 总字节数溢出".into()))?;
        file_count = file_count
            .checked_add(1)
            .ok_or_else(|| AppError::Other("ZIP 文件数溢出".into()))?;
        if file_count > u16::MAX as u32 {
            return Err(AppError::Other("ZIP32 最多支持 65535 个条目".into()));
        }
        // Local file header
        writer.write_all(&0x04034b50u32.to_le_bytes())?; // signature
        writer.write_all(&20u16.to_le_bytes())?; // version needed
        writer.write_all(&0u16.to_le_bytes())?; // flags
        writer.write_all(&0u16.to_le_bytes())?; // method STORE
        writer.write_all(&0u16.to_le_bytes())?; // mod time
        writer.write_all(&0u16.to_le_bytes())?; // mod date
        writer.write_all(&crc.to_le_bytes())?;
        writer.write_all(&size.to_le_bytes())?; // compressed size
        writer.write_all(&size.to_le_bytes())?; // uncompressed size
        writer.write_all(&name_len.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(rel_str.as_bytes())?;
        writer.write_all(&data)?;

        let local_header_size = 30u32
            .checked_add(rel_str.len() as u32)
            .ok_or_else(|| AppError::Other("ZIP local header 大小溢出".into()))?;
        central.push(CentralEntry {
            name: rel_str,
            crc,
            size,
            offset,
        });
        offset = offset
            .checked_add(local_header_size)
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| AppError::Other("ZIP32 local offset 溢出".into()))?;
    }

    // Central directory
    let cd_offset = offset;
    let mut cd_size: u32 = 0;
    for e in &central {
        let name_len = u16::try_from(e.name.len())
            .map_err(|_| AppError::Other(format!("ZIP central 文件名过长: {}", e.name)))?;
        writer.write_all(&0x02014b50u32.to_le_bytes())?; // signature
        writer.write_all(&20u16.to_le_bytes())?; // version made by
        writer.write_all(&20u16.to_le_bytes())?; // version needed
        writer.write_all(&0u16.to_le_bytes())?; // flags
        writer.write_all(&0u16.to_le_bytes())?; // method
        writer.write_all(&0u16.to_le_bytes())?; // mod time
        writer.write_all(&0u16.to_le_bytes())?; // mod date
        writer.write_all(&e.crc.to_le_bytes())?;
        writer.write_all(&e.size.to_le_bytes())?;
        writer.write_all(&e.size.to_le_bytes())?;
        writer.write_all(&name_len.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // extra len
        writer.write_all(&0u16.to_le_bytes())?; // comment len
        writer.write_all(&0u16.to_le_bytes())?; // disk start
        writer.write_all(&0u16.to_le_bytes())?; // int attrs
        writer.write_all(&0u32.to_le_bytes())?; // ext attrs
        writer.write_all(&e.offset.to_le_bytes())?;
        writer.write_all(e.name.as_bytes())?;
        cd_size = cd_size
            .checked_add(46)
            .and_then(|value| value.checked_add(e.name.len() as u32))
            .ok_or_else(|| AppError::Other("ZIP32 central directory 大小溢出".into()))?;
    }

    // EOCD
    writer.write_all(&0x06054b50u32.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // disk
    writer.write_all(&0u16.to_le_bytes())?; // cd start disk
    let entry_count = u16::try_from(central.len())
        .map_err(|_| AppError::Other("ZIP32 最多支持 65535 个条目".into()))?;
    writer.write_all(&entry_count.to_le_bytes())?;
    writer.write_all(&entry_count.to_le_bytes())?;
    writer.write_all(&cd_size.to_le_bytes())?;
    writer.write_all(&cd_offset.to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // comment len
    writer.flush()?;
    writer.get_ref().sync_all()?;

    Ok((file_count, total_bytes))
}

fn publish_packed_zip(stage: &Path, output: &Path) -> AppResult<()> {
    let stage_fingerprint = atomic_file::fingerprint(stage)?;
    let copy_stage = |file: &mut File| -> AppResult<()> {
        let mut source = File::open(stage)?;
        std::io::copy(&mut source, file)?;
        if atomic_file::fingerprint(stage)? != stage_fingerprint {
            return Err(AppError::Other(format!(
                "ZIP 暂存文件在发布期间发生变化: {}",
                stage.to_string_lossy()
            )));
        }
        Ok(())
    };
    match fs::symlink_metadata(output) {
        Ok(metadata) => {
            if !metadata.is_file() || crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "ZIP 输出在发布前不是普通文件或属于链接/reparse point: {}",
                    output.to_string_lossy()
                )));
            }
            let expected = atomic_file::fingerprint(output)?;
            atomic_file::replace_with_writer_if_unchanged(output, &expected, copy_stage)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_file::create_with_writer_if_absent(output, copy_stage)
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) struct CentralEntry {
    pub(super) name: String,
    pub(super) crc: u32,
    pub(super) size: u32,
    pub(super) offset: u32,
}

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            table[i as usize] = c;
        }
        table
    })
}

struct Crc32Hasher {
    value: u32,
}

impl Crc32Hasher {
    fn new() -> Self {
        Self { value: 0xFFFFFFFF }
    }

    fn update(&mut self, data: &[u8]) {
        let table = crc32_table();
        for &byte in data {
            let index = ((self.value ^ byte as u32) & 0xFF) as usize;
            self.value = table[index] ^ (self.value >> 8);
        }
    }

    fn finish(self) -> u32 {
        self.value ^ 0xFFFFFFFF
    }
}

pub(super) fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Crc32Hasher::new();
    hasher.update(data);
    hasher.finish()
}

fn checked_zip_slice<'a>(
    data: &'a [u8],
    start: usize,
    len: usize,
    label: &str,
) -> AppResult<&'a [u8]> {
    let end = start
        .checked_add(len)
        .ok_or_else(|| AppError::Other(format!("{label} 偏移溢出")))?;
    data.get(start..end)
        .ok_or_else(|| AppError::Other(format!("{label} 超出 zip 文件边界")))
}

fn read_zip_u16(data: &[u8], start: usize, label: &str) -> AppResult<u16> {
    let bytes = checked_zip_slice(data, start, 2, label)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_zip_u32(data: &[u8], start: usize, label: &str) -> AppResult<u32> {
    let bytes = checked_zip_slice(data, start, 4, label)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[derive(Debug)]
struct ParsedZipEntry {
    name: String,
    relative_path: PathBuf,
    is_directory: bool,
    crc32: u32,
    size: u64,
    payload_start: u64,
    local_range_end: u64,
    local_offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZipDestinationState {
    Missing,
    EmptyDirectory,
}

fn validate_plain_directory_metadata(path: &Path, label: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
        return Err(AppError::Path(format!(
            "{label} 包含符号链接、junction 或 reparse point，已拒绝: {}",
            path.to_string_lossy()
        )));
    }
    if !metadata.is_dir() {
        return Err(AppError::Path(format!(
            "{label} 必须是普通目录: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

fn absolute_lexical_path(path: &Path, label: &str) -> AppResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::Path(format!(
                        "{label} 包含无法解析的父目录跳转: {}",
                        path.to_string_lossy()
                    )));
                }
            }
        }
    }
    if normalized.file_name().is_none() {
        return Err(AppError::Path(format!(
            "{label} 不能指向文件系统根目录: {}",
            path.to_string_lossy()
        )));
    }
    Ok(normalized)
}

fn validate_existing_directory_chain(path: &Path, label: &str) -> AppResult<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    let mut missing_parent_seen = false;
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                if missing_parent_seen {
                    return Err(AppError::Path(format!(
                        "{label} 在缺失父目录之后出现已有条目: {}",
                        ancestor.to_string_lossy()
                    )));
                }
                validate_plain_directory_metadata(ancestor, label)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_parent_seen = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_created_directories(created: &[PathBuf]) -> Vec<String> {
    let mut errors = Vec::new();
    for directory in created.iter().rev() {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!(
                "清理新建父目录失败 {}: {error}",
                directory.to_string_lossy()
            )),
        }
    }
    errors
}

fn ensure_plain_directory_chain(path: &Path, label: &str) -> AppResult<Vec<PathBuf>> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    ancestors.reverse();
    let mut created = Vec::new();
    for ancestor in ancestors {
        let result = match fs::symlink_metadata(&ancestor) {
            Ok(_) => validate_plain_directory_metadata(&ancestor, label),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&ancestor) {
                    Ok(()) => created.push(ancestor.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        let cleanup = remove_created_directories(&created);
                        let suffix = if cleanup.is_empty() {
                            String::new()
                        } else {
                            format!("；{}", cleanup.join("；"))
                        };
                        return Err(AppError::Other(format!(
                            "创建 {label} 失败 {}: {error}{suffix}",
                            ancestor.to_string_lossy()
                        )));
                    }
                }
                validate_plain_directory_metadata(&ancestor, label)
            }
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            let cleanup = remove_created_directories(&created);
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
        }
    }
    if let Err(error) = validate_existing_directory_chain(path, label) {
        let cleanup = remove_created_directories(&created);
        if cleanup.is_empty() {
            return Err(error);
        }
        return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
    }
    Ok(created)
}

fn inspect_zip_destination(path: &Path) -> AppResult<ZipDestinationState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标不能是符号链接、junction 或 reparse point: {}",
                    path.to_string_lossy()
                )));
            }
            if !metadata.is_dir() {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标已存在且不是目录: {}",
                    path.to_string_lossy()
                )));
            }
            if fs::read_dir(path)?.next().transpose()?.is_some() {
                return Err(AppError::Path(format!(
                    "ZIP 解包目标已存在且非空，拒绝覆盖: {}",
                    path.to_string_lossy()
                )));
            }
            Ok(ZipDestinationState::EmptyDirectory)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ZipDestinationState::Missing)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_zip_flags(flags: u16, name: &str, header: &str) -> AppResult<()> {
    const UTF8_FLAG: u16 = 1 << 11;
    if flags & !UTF8_FLAG != 0 {
        return Err(AppError::Other(format!(
            "{header} 含不支持的 ZIP flags=0x{flags:04x}: {name}"
        )));
    }
    Ok(())
}

fn validate_zip_entry_attributes(
    name: &str,
    version_made_by: u16,
    external_attributes: u32,
    is_directory: bool,
) -> AppResult<()> {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    if external_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(AppError::Path(format!(
            "ZIP 条目标记为 reparse point，已拒绝: {name}"
        )));
    }
    if external_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !is_directory {
        return Err(AppError::Other(format!(
            "ZIP 条目的目录属性与文件名不一致: {name}"
        )));
    }

    let creator_system = (version_made_by >> 8) as u8;
    if creator_system == 3 {
        let unix_mode = (external_attributes >> 16) as u16;
        match unix_mode & 0o170000 {
            0 => {}
            0o040000 if is_directory => {}
            0o100000 if !is_directory => {}
            0o120000 => {
                return Err(AppError::Path(format!(
                    "ZIP 条目是符号链接，已拒绝: {name}"
                )))
            }
            _ => {
                return Err(AppError::Path(format!(
                    "ZIP 条目不是普通文件或目录，已拒绝: {name}"
                )))
            }
        }
    }
    Ok(())
}

fn checked_zip_entry_path(name: &str, is_directory: bool) -> AppResult<(PathBuf, String)> {
    if name != name.trim() || name.contains('\\') {
        return Err(AppError::Path(format!(
            "ZIP 条目路径包含歧义空白或反斜杠，已拒绝: {name}"
        )));
    }
    let path_text = if is_directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if path_text.is_empty()
        || path_text
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(AppError::Path(format!(
            "ZIP 条目路径无效或包含目录穿越: {name}"
        )));
    }
    #[cfg(windows)]
    for segment in path_text.split('/') {
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Err(AppError::Path(format!(
                "ZIP 条目在 Windows 上具有歧义的尾随空格或点: {name}"
            )));
        }
    }
    let relative = paths::checked_relative_path(path_text)?;
    let mut key = relative.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        key = key.to_lowercase();
    }
    Ok((relative, key))
}

fn parse_zip_entries(file: &mut File, file_len: u64) -> AppResult<Vec<ParsedZipEntry>> {
    if file_len < 22 {
        return Err(AppError::Other("不是合法的 zip 文件（过短）".into()));
    }

    let tail_window = file_len.min(65_557);
    let tail_base = file_len - tail_window;
    file.seek(SeekFrom::Start(tail_base))?;
    let tail_len = usize::try_from(tail_window)
        .map_err(|_| AppError::Other("ZIP 尾部窗口长度超出平台限制".into()))?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)?;
    let eocd_signature = [0x50u8, 0x4b, 0x05, 0x06];
    let eocd_in_tail = (0..=tail.len() - 22)
        .rev()
        .find(|&index| {
            if tail[index..index + 4] != eocd_signature {
                return false;
            }
            let comment_len = u16::from_le_bytes([tail[index + 20], tail[index + 21]]) as usize;
            index
                .checked_add(22)
                .and_then(|value| value.checked_add(comment_len))
                == Some(tail.len())
        })
        .ok_or_else(|| AppError::Other("不是合法的 zip 文件（未找到有效 EOCD）".into()))?;
    let eocd_offset = tail_base
        .checked_add(eocd_in_tail as u64)
        .ok_or_else(|| AppError::Other("EOCD 偏移溢出".into()))?;
    let disk_number = read_zip_u16(&tail, eocd_in_tail + 4, "EOCD disk number")?;
    let central_disk = read_zip_u16(&tail, eocd_in_tail + 6, "EOCD central disk")?;
    let disk_entry_count = read_zip_u16(&tail, eocd_in_tail + 8, "EOCD 当前磁盘条目数")?;
    let entry_count = read_zip_u16(&tail, eocd_in_tail + 10, "EOCD 总条目数")?;
    let central_size = read_zip_u32(&tail, eocd_in_tail + 12, "central directory 总大小")?;
    let central_offset = read_zip_u32(&tail, eocd_in_tail + 16, "central directory 偏移")?;
    if disk_number != 0 || central_disk != 0 || disk_entry_count != entry_count {
        return Err(AppError::Other("不支持分卷 ZIP".into()));
    }
    if entry_count == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(AppError::Other("不支持 ZIP64".into()));
    }
    let central_offset = central_offset as u64;
    let central_size = central_size as u64;
    if central_offset.checked_add(central_size) != Some(eocd_offset) {
        return Err(AppError::Other(format!(
            "central directory 范围与 EOCD 不一致: offset={central_offset} size={central_size} eocd={eocd_offset}"
        )));
    }
    let central_len = usize::try_from(central_size)
        .map_err(|_| AppError::Other("central directory 大小超出平台限制".into()))?;
    file.seek(SeekFrom::Start(central_offset))?;
    let mut central = vec![0u8; central_len];
    file.read_exact(&mut central)?;

    let mut position = 0usize;
    let mut entries = Vec::with_capacity(entry_count as usize);
    let mut path_kinds = HashMap::<String, bool>::new();
    for _ in 0..entry_count {
        if checked_zip_slice(&central, position, 4, "central directory 签名")?
            != [0x50, 0x4b, 0x01, 0x02]
        {
            return Err(AppError::Other("central directory 损坏".into()));
        }
        checked_zip_slice(&central, position, 46, "central directory header")?;
        let version_made_by =
            read_zip_u16(&central, position + 4, "central directory version made by")?;
        let flags = read_zip_u16(&central, position + 8, "central directory flags")?;
        let method = read_zip_u16(&central, position + 10, "central directory 压缩方式")?;
        let crc = read_zip_u32(&central, position + 16, "central directory CRC")?;
        let compressed_size =
            read_zip_u32(&central, position + 20, "central directory 压缩后大小")? as u64;
        let uncompressed_size =
            read_zip_u32(&central, position + 24, "central directory 原始大小")? as u64;
        let name_len =
            read_zip_u16(&central, position + 28, "central directory 文件名长度")? as usize;
        let extra_len =
            read_zip_u16(&central, position + 30, "central directory extra 长度")? as usize;
        let comment_len =
            read_zip_u16(&central, position + 32, "central directory 注释长度")? as usize;
        let disk_start = read_zip_u16(&central, position + 34, "central directory disk start")?;
        let external_attributes =
            read_zip_u32(&central, position + 38, "central directory 外部属性")?;
        let local_offset = read_zip_u32(&central, position + 42, "local header 偏移")? as u64;
        let name_bytes = checked_zip_slice(
            &central,
            position + 46,
            name_len,
            "central directory 文件名",
        )?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|error| AppError::Other(format!("zip 文件名不是 UTF-8: {error}")))?;
        let advance = 46usize
            .checked_add(name_len)
            .and_then(|value| value.checked_add(extra_len))
            .and_then(|value| value.checked_add(comment_len))
            .ok_or_else(|| AppError::Other("central directory entry 长度溢出".into()))?;
        checked_zip_slice(&central, position, advance, "central directory entry")?;
        position = position
            .checked_add(advance)
            .ok_or_else(|| AppError::Other("central directory 游标溢出".into()))?;

        if disk_start != 0 {
            return Err(AppError::Other(format!("ZIP 条目位于其他分卷: {name}")));
        }
        validate_zip_flags(flags, &name, "central directory")?;
        if method != 0 {
            return Err(AppError::Other(format!(
                "不支持的压缩方式 method={method}（仅支持 STORE）: {name}"
            )));
        }
        if compressed_size != uncompressed_size {
            return Err(AppError::Other(format!(
                "STORE 条目的压缩大小与原始大小不一致: {name}"
            )));
        }
        let is_directory = name.ends_with('/');
        if is_directory && (compressed_size != 0 || crc != 0) {
            return Err(AppError::Other(format!(
                "ZIP 目录条目声明了 payload 或非零 CRC: {name}"
            )));
        }
        validate_zip_entry_attributes(&name, version_made_by, external_attributes, is_directory)?;
        let (relative_path, path_key) = checked_zip_entry_path(&name, is_directory)?;
        if path_kinds.insert(path_key.clone(), is_directory).is_some() {
            return Err(AppError::Path(format!(
                "ZIP 包含重复或等价路径，已拒绝: {name}"
            )));
        }

        let local_header_end = local_offset
            .checked_add(30)
            .ok_or_else(|| AppError::Other(format!("local header 偏移溢出: {name}")))?;
        if local_header_end > central_offset {
            return Err(AppError::Other(format!("local header 越界: {name}")));
        }
        file.seek(SeekFrom::Start(local_offset))?;
        let mut local_header = [0u8; 30];
        file.read_exact(&mut local_header)?;
        if local_header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(AppError::Other(format!("local header 损坏: {name}")));
        }
        let local_flags = read_zip_u16(&local_header, 6, "local header flags")?;
        let local_method = read_zip_u16(&local_header, 8, "local header 压缩方式")?;
        let local_crc = read_zip_u32(&local_header, 14, "local header CRC")?;
        let local_compressed_size =
            read_zip_u32(&local_header, 18, "local header 压缩后大小")? as u64;
        let local_uncompressed_size =
            read_zip_u32(&local_header, 22, "local header 原始大小")? as u64;
        let local_name_len = read_zip_u16(&local_header, 26, "local header 文件名长度")? as u64;
        let local_extra_len = read_zip_u16(&local_header, 28, "local header extra 长度")? as u64;
        validate_zip_flags(local_flags, &name, "local header")?;
        if local_flags != flags
            || local_method != method
            || local_crc != crc
            || local_compressed_size != compressed_size
            || local_uncompressed_size != uncompressed_size
        {
            return Err(AppError::Other(format!(
                "central directory 与 local header 的 flags/method/CRC/size 不一致: {name}"
            )));
        }
        let local_name_start = local_offset + 30;
        let payload_start = local_name_start
            .checked_add(local_name_len)
            .and_then(|value| value.checked_add(local_extra_len))
            .ok_or_else(|| AppError::Other(format!("payload 偏移溢出: {name}")))?;
        let payload_end = payload_start
            .checked_add(compressed_size)
            .ok_or_else(|| AppError::Other(format!("payload 范围溢出: {name}")))?;
        if payload_end > central_offset {
            return Err(AppError::Other(format!("payload 范围越界: {name}")));
        }
        let local_name_len_usize = usize::try_from(local_name_len)
            .map_err(|_| AppError::Other(format!("local header 文件名过长: {name}")))?;
        file.seek(SeekFrom::Start(local_name_start))?;
        let mut local_name = vec![0u8; local_name_len_usize];
        file.read_exact(&mut local_name)?;
        if local_name != name_bytes {
            return Err(AppError::Other(format!(
                "central directory 与 local header 的文件名不一致: {name}"
            )));
        }

        entries.push(ParsedZipEntry {
            name,
            relative_path,
            is_directory,
            crc32: crc,
            size: compressed_size,
            payload_start,
            local_range_end: payload_end,
            local_offset,
        });
    }
    if position != central.len() {
        return Err(AppError::Other(format!(
            "central directory 条目数量或总大小不一致: 已解析 {position} 字节，声明 {} 字节",
            central.len()
        )));
    }

    for path in path_kinds.keys() {
        let mut parent = path.as_str();
        while let Some(separator) = parent.rfind('/') {
            parent = &parent[..separator];
            if path_kinds.get(parent) == Some(&false) {
                return Err(AppError::Path(format!(
                    "ZIP 文件条目同时被用作父目录，已拒绝: {parent} -> {path}"
                )));
            }
        }
    }

    let mut ranges = entries
        .iter()
        .map(|entry| {
            (
                entry.local_offset,
                entry.local_range_end,
                entry.name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(AppError::Other(format!(
                "ZIP local header/payload 范围重叠: {} 与 {}",
                pair[0].2, pair[1].2
            )));
        }
    }
    Ok(entries)
}

fn create_unique_stage_directory(parent: &Path) -> AppResult<PathBuf> {
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".ccsm-unpack-{}-{sequence}.stage",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(error) = validate_plain_directory_metadata(&path, "ZIP 解包暂存目录")
                {
                    return match fs::remove_dir(&path) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(AppError::Other(format!(
                            "{error}；清理新建 ZIP 暂存目录失败 {}: {cleanup_error}",
                            path.to_string_lossy()
                        ))),
                    };
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn ensure_plain_stage_subdirectory(stage: &Path, relative: &Path) -> AppResult<PathBuf> {
    let mut current = stage.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(AppError::Path(format!(
                "ZIP 暂存相对目录包含非法组件: {}",
                relative.to_string_lossy()
            )));
        };
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(_) => validate_plain_directory_metadata(&current, "ZIP 暂存子目录")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                validate_plain_directory_metadata(&current, "ZIP 暂存子目录")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn extract_zip_entries_to_stage(
    file: &mut File,
    entries: &[ParsedZipEntry],
    stage: &Path,
) -> AppResult<(u32, u64)> {
    let mut file_count = 0u32;
    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    for entry in entries {
        if entry.is_directory {
            ensure_plain_stage_subdirectory(stage, &entry.relative_path)?;
            continue;
        }
        let parent_relative = entry
            .relative_path
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let parent = ensure_plain_stage_subdirectory(stage, parent_relative)?;
        let file_name = entry
            .relative_path
            .file_name()
            .ok_or_else(|| AppError::Path(format!("ZIP 文件条目缺少文件名: {}", entry.name)))?;
        let output_path = parent.join(file_name);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)?;
        let output_metadata = output.metadata()?;
        if !output_metadata.is_file()
            || crate::path_safety::metadata_is_link_or_reparse(&output_metadata)
        {
            return Err(AppError::Path(format!(
                "ZIP 暂存输出不是普通文件: {}",
                output_path.to_string_lossy()
            )));
        }

        file.seek(SeekFrom::Start(entry.payload_start))?;
        let mut remaining = entry.size;
        let mut hasher = Crc32Hasher::new();
        while remaining > 0 {
            let requested = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| AppError::Other("ZIP payload 读取长度超出平台限制".into()))?;
            let read = file.read(&mut buffer[..requested])?;
            if read == 0 {
                return Err(AppError::Other(format!(
                    "ZIP payload 提前结束，仍缺少 {remaining} 字节: {}",
                    entry.name
                )));
            }
            output.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        output.flush()?;
        output.sync_all()?;
        let actual_crc = hasher.finish();
        if actual_crc != entry.crc32 {
            return Err(AppError::Other(format!(
                "ZIP 条目 CRC 校验失败: {}，声明 {:08x}，实际 {:08x}",
                entry.name, entry.crc32, actual_crc
            )));
        }
        let written_metadata = fs::symlink_metadata(&output_path)?;
        if !written_metadata.is_file()
            || crate::path_safety::metadata_is_link_or_reparse(&written_metadata)
        {
            return Err(AppError::Path(format!(
                "ZIP 暂存文件在写入期间被替换: {}",
                output_path.to_string_lossy()
            )));
        }
        if written_metadata.len() != entry.size {
            return Err(AppError::Other(format!(
                "ZIP 暂存文件大小不一致: {}，声明 {}，实际 {}",
                entry.name,
                entry.size,
                written_metadata.len()
            )));
        }
        total_bytes = total_bytes
            .checked_add(entry.size)
            .ok_or_else(|| AppError::Other("ZIP 解包总字节数溢出".into()))?;
        file_count = file_count
            .checked_add(1)
            .ok_or_else(|| AppError::Other("ZIP 解包文件数溢出".into()))?;
    }
    Ok((file_count, total_bytes))
}

fn unique_missing_sibling(parent: &Path, suffix: &str) -> AppResult<PathBuf> {
    loop {
        let sequence = ZIP_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".ccsm-unpack-{}-{sequence}.{suffix}",
            std::process::id()
        ));
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn publish_zip_stage(
    stage: &Path,
    destination: &Path,
    parent: &Path,
    initial_state: ZipDestinationState,
) -> AppResult<()> {
    validate_existing_directory_chain(parent, "ZIP 解包目标父目录")?;
    match initial_state {
        ZipDestinationState::Missing => {
            if inspect_zip_destination(destination)? != ZipDestinationState::Missing {
                return Err(AppError::Other(format!(
                    "ZIP 解包目标在发布前被其他进程创建: {}",
                    destination.to_string_lossy()
                )));
            }
            fs::rename(stage, destination)?;
        }
        ZipDestinationState::EmptyDirectory => {
            if inspect_zip_destination(destination)? != ZipDestinationState::EmptyDirectory {
                return Err(AppError::Other(format!(
                    "ZIP 解包目标在发布前发生变化: {}",
                    destination.to_string_lossy()
                )));
            }
            let backup = unique_missing_sibling(parent, "empty-destination")?;
            fs::rename(destination, &backup)?;
            if let Err(publish_error) = fs::rename(stage, destination) {
                let restore_error = fs::rename(&backup, destination).err();
                return match restore_error {
                    None => Err(publish_error.into()),
                    Some(restore_error) => Err(AppError::Other(format!(
                        "发布 ZIP 解包结果失败: {publish_error}；恢复原空目标也失败: {restore_error}"
                    ))),
                };
            }
            if let Err(cleanup_error) = fs::remove_dir(&backup) {
                let park_result = fs::rename(destination, stage);
                let restore_result = fs::rename(&backup, destination);
                return match (park_result, restore_result) {
                    (Ok(()), Ok(())) => Err(AppError::Other(format!(
                        "清理原空目标暂存目录失败，已回滚发布: {cleanup_error}"
                    ))),
                    (park, restore) => Err(AppError::Other(format!(
                        "清理原空目标暂存目录失败: {cleanup_error}；回滚新目标结果={park:?}；恢复原目标结果={restore:?}"
                    ))),
                };
            }
        }
    }
    Ok(())
}

fn cleanup_failed_zip_unpack(
    stage: &Path,
    created_parents: &[PathBuf],
    primary_error: AppError,
) -> AppError {
    let mut cleanup_errors = Vec::new();
    match fs::symlink_metadata(stage) {
        Ok(metadata) => {
            if crate::path_safety::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                cleanup_errors.push(format!(
                    "暂存路径不再是普通目录，拒绝递归清理: {}",
                    stage.to_string_lossy()
                ));
            } else if let Err(error) = fs::remove_dir_all(stage) {
                cleanup_errors.push(format!(
                    "清理 ZIP 暂存目录失败 {}: {error}",
                    stage.to_string_lossy()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => cleanup_errors.push(format!(
            "检查 ZIP 暂存目录失败 {}: {error}",
            stage.to_string_lossy()
        )),
    }
    cleanup_errors.extend(remove_created_directories(created_parents));
    if cleanup_errors.is_empty() {
        primary_error
    } else {
        AppError::Other(format!(
            "{primary_error}；清理失败：{}",
            cleanup_errors.join("；")
        ))
    }
}

pub fn unpack_zip(zip_path: String, dst_dir: String) -> AppResult<ZipReport> {
    let destination = absolute_lexical_path(Path::new(&dst_dir), "ZIP 解包目标")?;
    let parent = destination.parent().ok_or_else(|| {
        AppError::Path(format!(
            "ZIP 解包目标缺少父目录: {}",
            destination.to_string_lossy()
        ))
    })?;
    validate_existing_directory_chain(parent, "ZIP 解包目标父目录")?;
    let initial_state = inspect_zip_destination(&destination)?;

    let zip_source = PathBuf::from(&zip_path);
    let source_metadata = fs::symlink_metadata(&zip_source)?;
    if crate::path_safety::metadata_is_link_or_reparse(&source_metadata)
        || !source_metadata.is_file()
    {
        return Err(AppError::Path(format!(
            "ZIP 源必须是普通文件且不能是链接或 reparse point: {}",
            zip_source.to_string_lossy()
        )));
    }
    let mut file = File::open(&zip_source)?;
    let file_len = file.metadata()?.len();
    let entries = parse_zip_entries(&mut file, file_len)?;

    let created_parents = ensure_plain_directory_chain(parent, "ZIP 解包目标父目录")?;
    if let Err(error) = match (initial_state, inspect_zip_destination(&destination)) {
        (ZipDestinationState::Missing, Ok(ZipDestinationState::Missing))
        | (ZipDestinationState::EmptyDirectory, Ok(ZipDestinationState::EmptyDirectory)) => Ok(()),
        (_, Ok(_)) => Err(AppError::Other(format!(
            "ZIP 解包目标在验证期间发生变化: {}",
            destination.to_string_lossy()
        ))),
        (_, Err(error)) => Err(error),
    } {
        let cleanup = remove_created_directories(&created_parents);
        if cleanup.is_empty() {
            return Err(error);
        }
        return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
    }

    let stage = match create_unique_stage_directory(parent) {
        Ok(stage) => stage,
        Err(error) => {
            let cleanup = remove_created_directories(&created_parents);
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(AppError::Other(format!("{error}；{}", cleanup.join("；"))));
        }
    };
    let (file_count, total_bytes) = match extract_zip_entries_to_stage(&mut file, &entries, &stage)
    {
        Ok(report) => report,
        Err(error) => return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error)),
    };
    if let Err(error) = crate::path_safety::validate_tree(parent, &stage, "ZIP 解包暂存树") {
        return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error));
    }
    if let Err(error) = publish_zip_stage(&stage, &destination, parent, initial_state) {
        return Err(cleanup_failed_zip_unpack(&stage, &created_parents, error));
    }

    Ok(ZipReport {
        path: destination.to_string_lossy().into_owned(),
        files: file_count,
        bytes: total_bytes,
    })
}

pub fn unpack_zip_to_temp(zip_path: String) -> AppResult<ZipReport> {
    let dir = std::env::temp_dir().join(format!(
        "cc-session-manager-import-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    unpack_zip(zip_path, dir.to_string_lossy().into_owned())
}
