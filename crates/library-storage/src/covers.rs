//! Проверка размера изображений и сохранение пользовательских обложек.
use crate::{Error, Result, disk};
use image::{ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug)]
pub struct CoverLimits {
    pub max_bytes: usize,
    pub max_dimension: u32,
    pub max_pixels: u64,
}

impl Default for CoverLimits {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            max_dimension: 8192,
            max_pixels: 16_000_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredCover {
    pub path: PathBuf,
    pub mime: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct CoverStore {
    root: PathBuf,
    limits: CoverLimits,
}

impl CoverStore {
    pub fn open(root: impl AsRef<Path>, limits: CoverLimits) -> Result<Self> {
        if limits.max_bytes == 0 || limits.max_dimension == 0 || limits.max_pixels == 0 {
            return Err(Error::Invalid(
                "лимиты обложек должны быть положительными".into(),
            ));
        }
        Ok(Self {
            root: disk::ensure_dir(root.as_ref())?,
            limits,
        })
    }

    pub fn put(&self, id: &str, bytes: &[u8]) -> Result<StoredCover> {
        disk::validate_id(id)?;
        let (format, width, height) = validate_cover(bytes, self.limits)?;
        let (extension, mime) = match format {
            ImageFormat::Png => ("png", "image/png"),
            ImageFormat::Jpeg => ("jpg", "image/jpeg"),
            ImageFormat::WebP => ("webp", "image/webp"),
            _ => unreachable!(),
        };
        // Содержимое входит в имя: неудачная запись каталога не меняет старую обложку.
        use sha2::{Digest, Sha256};
        let path = self.root.join(format!(
            "{}-{:x}.{extension}",
            disk::key(id),
            Sha256::digest(bytes)
        ));
        disk::atomic_write(&path, bytes)?;
        Ok(StoredCover {
            path,
            mime: mime.into(),
            bytes: bytes.len() as u64,
            width,
            height,
        })
    }
}

pub(crate) fn validate_cover(bytes: &[u8], limits: CoverLimits) -> Result<(ImageFormat, u32, u32)> {
    if bytes.is_empty() || bytes.len() > limits.max_bytes {
        return Err(Error::Limit("размер обложки".into()));
    }
    let format = image::guess_format(bytes)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(Error::Invalid("разрешены только PNG, JPEG и WebP".into()));
    }
    let dimensions = ImageReader::with_format(Cursor::new(bytes), format).into_dimensions()?;
    let (width, height) = dimensions;
    if width == 0
        || height == 0
        || width > limits.max_dimension
        || height > limits.max_dimension
        || u64::from(width) * u64::from(height) > limits.max_pixels
    {
        return Err(Error::Limit("размеры изображения".into()));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_dimension);
    decode_limits.max_image_height = Some(limits.max_dimension);
    decode_limits.max_alloc = Some(limits.max_pixels.saturating_mul(16));
    reader.limits(decode_limits);
    // Проверка заголовка недостаточна: декодируем данные с ограничением памяти.
    reader.decode()?;
    Ok((format, width, height))
}
