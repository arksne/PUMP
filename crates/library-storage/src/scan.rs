//! Рекурсивный поиск поддерживаемых аудиофайлов с отказом от symlink.
use crate::{
    Error, Result, disk,
    tags::{self, AudioTags},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions {
    pub max_entries: usize,
    pub include_hidden: bool,
}
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            include_hidden: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScannedTrack {
    pub id: String,
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub bytes: u64,
    pub tags: Option<AudioTags>,
    pub tag_error: Option<String>,
}

/// Обходит обычные файлы; ссылки на файлы и папки не включаются в результат.
pub fn scan_library(root: impl AsRef<Path>, options: ScanOptions) -> Result<Vec<ScannedTrack>> {
    disk::no_symlinks(root.as_ref())?;
    let root = root.as_ref().canonicalize()?;
    if !root.is_dir() {
        return Err(Error::Invalid("корень медиатеки должен быть папкой".into()));
    }
    let mut stack = vec![root.clone()];
    let mut output = Vec::new();
    let mut visited = 0;
    while let Some(dir) = stack.pop() {
        disk::no_symlinks(&dir)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            visited += 1;
            if visited > options.max_entries {
                return Err(Error::Limit("число элементов при сканировании".into()));
            }
            if !options.include_hidden && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if disk::validate_extension(&extension).is_err() {
                continue;
            }
            disk::no_symlinks(&path)?;
            let bytes = entry.metadata()?.len();
            if bytes == 0 {
                continue;
            }
            let relative_path = path
                .strip_prefix(&root)
                .map_err(|_| Error::Invalid("выход за корень".into()))?
                .to_path_buf();
            use sha2::{Digest, Sha256};
            let id = format!(
                "{:x}",
                Sha256::digest(relative_path.as_os_str().as_encoded_bytes())
            );
            let (tags, tag_error) = match tags::read_tags(&path) {
                Ok(tags) => (Some(tags), None),
                Err(e) => (None, Some(e.to_string())),
            };
            output.push(ScannedTrack {
                id,
                path,
                relative_path,
                bytes,
                tags,
                tag_error,
            });
        }
    }
    output.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(output)
}
