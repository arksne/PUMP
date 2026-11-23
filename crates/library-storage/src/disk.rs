//! Атомарная запись файлов и проверка путей локального хранилища.
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

pub(crate) fn key(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 4096 || id.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "идентификатор пуст, слишком длинен или содержит управляющие символы".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_extension(ext: &str) -> Result<()> {
    if !matches!(
        ext,
        "mp3" | "m4a" | "aac" | "ogg" | "opus" | "flac" | "wav" | "aiff" | "ape" | "mp4" | "webm"
    ) {
        return Err(Error::Invalid("неподдерживаемое расширение аудио".into()));
    }
    Ok(())
}

pub(crate) fn no_symlinks(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(Error::Invalid("компонент .. запрещён".into()));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::Invalid("символические ссылки запрещены".into()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) fn ensure_dir(path: &Path) -> Result<PathBuf> {
    no_symlinks(path)?;
    fs::create_dir_all(path)?;
    Ok(path.canonicalize()?)
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    no_symlinks(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("нет родительской папки".into()))?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(crate) fn validate_url(url: &url::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Invalid(
            "нужен HTTP(S) URL без встроенных учётных данных и фрагмента".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_metadata(metadata: &crate::TrackMetadata) -> Result<()> {
    for text in [&metadata.title, &metadata.artist, &metadata.album]
        .into_iter()
        .flatten()
    {
        if text.len() > 16 * 1024 {
            return Err(Error::Limit("метаданные".into()));
        }
    }
    if let Some(lyrics) = &metadata.lyrics {
        for text in [&lyrics.plain, &lyrics.synced].into_iter().flatten() {
            if text.len() > 1024 * 1024 {
                return Err(Error::Limit("текст песни".into()));
            }
        }
    }
    Ok(())
}
