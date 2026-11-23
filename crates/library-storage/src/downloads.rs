//! Атомарная загрузка аудио, сохранение очереди и повтор незавершённых заданий.
use crate::{
    DownloadJob, DownloadProgress, DownloadRequest, Error, JobStatus, LibraryTrack, NetworkMode,
    Result, disk,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore, broadcast},
};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub concurrency: usize,
    pub max_bytes: u64,
    pub mode: NetworkMode,
}
#[derive(Clone)]
pub struct DownloadManager {
    _lock: Arc<std::fs::File>,
    config: StorageConfig,
    client: reqwest::Client,
    state: Arc<Mutex<crate::model::Catalog>>,
    slots: Arc<Semaphore>,
    updates: broadcast::Sender<DownloadProgress>,
}
impl DownloadManager {
    pub async fn open(config: StorageConfig) -> Result<Self> {
        if config.concurrency == 0 || config.max_bytes == 0 {
            return Err(Error::Invalid("нулевой лимит загрузок".into()));
        }
        disk::ensure_dir(&config.root)?;
        use fs2::FileExt;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.root.join(".lock"))?;
        lock.try_lock_exclusive().map_err(|_| Error::Busy)?;
        let path = config.root.join("catalog.json");
        let mut state: crate::model::Catalog = if path.exists() {
            serde_json::from_slice(&tokio::fs::read(path).await?)?
        } else {
            Default::default()
        };
        for job in state.jobs.values_mut() {
            if job.status == JobStatus::Downloading {
                job.status = JobStatus::Pending;
            }
        }
        let (updates, _) = broadcast::channel(64);
        Ok(Self {
            _lock: Arc::new(lock),
            slots: Arc::new(Semaphore::new(config.concurrency)),
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()?,
            state: Arc::new(Mutex::new(state)),
            updates,
        })
    }
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadProgress> {
        self.updates.subscribe()
    }
    pub async fn jobs(&self) -> Vec<DownloadJob> {
        self.state.lock().await.jobs.values().cloned().collect()
    }
    pub async fn tracks(&self) -> Vec<LibraryTrack> {
        self.state.lock().await.tracks.values().cloned().collect()
    }
    async fn save(&self, state: &crate::model::Catalog) -> Result<()> {
        disk::atomic_write(
            &self.config.root.join("catalog.json"),
            &serde_json::to_vec(state)?,
        )
    }
    pub async fn enqueue(&self, request: DownloadRequest) -> Result<()> {
        self.config.mode.require_online()?;
        disk::validate_id(&request.id)?;
        disk::validate_url(&request.url)?;
        if let Some(url) = &request.cover_url {
            disk::validate_url(url)?;
        }
        disk::validate_extension(&request.extension)?;
        disk::validate_metadata(&request.metadata)?;
        let mut state = self.state.lock().await;
        if state.jobs.contains_key(&request.id) {
            return Err(Error::AlreadyExists(request.id));
        }
        state.jobs.insert(
            request.id.clone(),
            DownloadJob {
                request,
                status: JobStatus::Pending,
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
            },
        );
        self.save(&state).await
    }
    pub async fn run(&self, id: &str, cancel: CancellationToken) -> Result<LibraryTrack> {
        self.config.mode.require_online()?;
        let _slot = tokio::select! {permit=self.slots.acquire()=>permit.map_err(|_|Error::Busy)?,_=cancel.cancelled()=>return Err(Error::Cancelled)};
        let request = {
            let mut state = self.state.lock().await;
            let job = state
                .jobs
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(id.into()))?;
            if job.status == JobStatus::Downloading {
                return Err(Error::Busy);
            }
            if job.status == JobStatus::Completed {
                return state
                    .tracks
                    .get(id)
                    .cloned()
                    .ok_or_else(|| Error::NotFound(id.into()));
            }
            job.status = JobStatus::Downloading;
            let r = job.request.clone();
            self.save(&state).await?;
            r
        };
        let path = self
            .config
            .root
            .join(format!("{}.{}", disk::key(id), request.extension));
        let tmp = self
            .config
            .root
            .join(format!(".{}.part", uuid::Uuid::new_v4()));
        let outcome = self.transfer(&request, &tmp, &path, &cancel).await;
        if outcome.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        let mut state = self.state.lock().await;
        let job = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(id.into()))?;
        match &outcome {
            Ok(track) => {
                job.status = JobStatus::Completed;
                job.downloaded_bytes = track.bytes;
                job.error = None;
            }
            Err(Error::Cancelled) => job.status = JobStatus::Cancelled,
            Err(e) => {
                job.status = JobStatus::Failed;
                job.error = Some(e.to_string());
            }
        }
        let _ = self.updates.send(DownloadProgress::from(&*job));
        if let Ok(track) = &outcome {
            state.tracks.insert(id.into(), track.clone());
        }
        self.save(&state).await?;
        outcome
    }
    async fn transfer(
        &self,
        request: &DownloadRequest,
        tmp: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<LibraryTrack> {
        let mut response = tokio::select! {r=self.client.get(request.url.clone()).send()=>r?.error_for_status()?,_=cancel.cancelled()=>return Err(Error::Cancelled)};
        let total = response.content_length();
        if total.is_some_and(|n| n > self.config.max_bytes) {
            return Err(Error::Limit("аудиофайл".into()));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
            .await?;
        let mut count = 0;
        loop {
            let chunk = tokio::select! {r=response.chunk()=>r?,_=cancel.cancelled()=>return Err(Error::Cancelled)};
            let Some(chunk) = chunk else {
                break;
            };
            count += chunk.len() as u64;
            if count > self.config.max_bytes {
                return Err(Error::Limit("аудиофайл".into()));
            }
            file.write_all(&chunk).await?;
            let _ = self.updates.send(DownloadProgress {
                id: request.id.clone(),
                status: JobStatus::Downloading,
                downloaded_bytes: count,
                total_bytes: total,
            });
        }
        file.sync_all().await?;
        drop(file);
        if count == 0 {
            return Err(Error::Invalid("пустой аудиофайл".into()));
        }
        let mut cover = None;
        let mut cover_bytes = None;
        if let Some(url) = &request.cover_url {
            let mut response = self
                .client
                .get(url.clone())
                .send()
                .await?
                .error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                    return Err(Error::Limit("обложка".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let store = crate::CoverStore::open(
                self.config.root.join("covers"),
                crate::CoverLimits::default(),
            )?;
            cover = Some(store.put(&request.id, &bytes)?);
            cover_bytes = Some(bytes);
        }
        let tag_path = tmp.to_owned();
        let meta = request.metadata.clone();
        tokio::task::spawn_blocking(move || {
            crate::tags::write_tags_with_cover(tag_path, &meta, cover_bytes.as_deref())
        })
        .await??;
        tokio::fs::rename(tmp, path).await?;
        disk::sync_dir(&self.config.root)?;
        Ok(LibraryTrack {
            id: request.id.clone(),
            audio_path: path.to_owned(),
            bytes: tokio::fs::metadata(path).await?.len(),
            metadata: request.metadata.clone(),
            cover,
        })
    }
    pub async fn playlist(
        &self,
        id: &str,
        title: &str,
        track_ids: Vec<String>,
    ) -> Result<crate::OfflineBundle> {
        disk::validate_id(id)?;
        let mut state = self.state.lock().await;
        let tracks = track_ids
            .iter()
            .map(|id| {
                state
                    .tracks
                    .get(id)
                    .cloned()
                    .ok_or_else(|| Error::NotFound(id.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        state.playlists.insert(
            id.into(),
            crate::OfflinePlaylist {
                id: id.into(),
                title: title.into(),
                track_ids,
            },
        );
        self.save(&state).await?;
        Ok(crate::OfflineBundle {
            id: id.into(),
            title: title.into(),
            tracks,
        })
    }
}
