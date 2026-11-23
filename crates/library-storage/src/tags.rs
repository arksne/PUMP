//! Чтение и атомарное обновление аудиотегов и встроенных обложек.
use crate::{Lyrics, Result, TrackMetadata, disk};
use lofty::{
    config::WriteOptions,
    file::{AudioFile, TaggedFileExt},
    probe::Probe,
    tag::{Accessor, ItemKey, Tag, TagExt},
};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct AudioTags {
    pub metadata: TrackMetadata,
    pub duration_ms: u64,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
}

pub fn read_tags(path: impl AsRef<Path>) -> Result<AudioTags> {
    disk::no_symlinks(path.as_ref())?;
    let file = Probe::open(path)?.guess_file_type()?.read()?;
    let mut metadata = TrackMetadata::default();
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        metadata.title = tag.title().map(|s| s.into_owned());
        metadata.artist = tag.artist().map(|s| s.into_owned());
        metadata.album = tag.album().map(|s| s.into_owned());
        metadata.lyrics = tag.get_string(&ItemKey::Lyrics).map(|s| Lyrics {
            plain: Some(s.into()),
            synced: None,
        });
    }
    Ok(AudioTags {
        metadata,
        duration_ms: file
            .properties()
            .duration()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        sample_rate: file.properties().sample_rate(),
        channels: file.properties().channels(),
    })
}

/// Применяет заданные поля; отсутствующие поля сохраняют прежнее значение.
/// Синхронизированный текст хранится в каталоге, обычный — также в аудиотегах.
pub fn write_tags(path: impl AsRef<Path>, metadata: &TrackMetadata) -> Result<()> {
    write_tags_with_cover(path, metadata, None)
}

pub fn write_tags_with_cover(
    path: impl AsRef<Path>,
    metadata: &TrackMetadata,
    cover: Option<&[u8]>,
) -> Result<()> {
    disk::validate_metadata(metadata)?;
    let path = path.as_ref();
    disk::no_symlinks(path)?;
    let mut file = Probe::open(path)?.guess_file_type()?.read()?;
    let tag_type = file.primary_tag_type();
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .primary_tag_mut()
        .ok_or_else(|| crate::Error::Invalid("формат не поддерживает теги".into()))?;
    if let Some(value) = &metadata.title {
        tag.set_title(value.clone());
    }
    if let Some(value) = &metadata.artist {
        tag.set_artist(value.clone());
    }
    if let Some(value) = &metadata.album {
        tag.set_album(value.clone());
    }
    if let Some(value) = metadata.lyrics.as_ref().and_then(|l| l.plain.as_ref()) {
        tag.insert_text(ItemKey::Lyrics, value.clone());
    }
    if let Some(bytes) = cover {
        let mut picture = lofty::picture::Picture::from_reader(&mut std::io::Cursor::new(bytes))?;
        picture.set_pic_type(lofty::picture::PictureType::CoverFront);
        tag.set_picture(0, picture);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp = parent.join(format!(".{}.tagtmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&tmp)?;
        let mut input = std::fs::File::open(path)?;
        std::io::copy(&mut input, &mut output)?;
        tag.save_to(&mut output, WriteOptions::default())?;
        output.set_permissions(input.metadata()?.permissions())?;
        output.sync_all()?;
        std::fs::rename(&tmp, path)?;
        disk::sync_dir(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
