//! Файловая очередь с эксклюзивной блокировкой и атомарной заменой снимка.
use crate::{
    Error, Result,
    lastfm::{ScrobbleSink, ScrobbleTrack, Submission},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingScrobble {
    pub key: String,
    pub track: ScrobbleTrack,
    pub attempts: u32,
    pub next_attempt_unix: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectedScrobble {
    pub entry: PendingScrobble,
    pub reason: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    version: u32,
    pending: Vec<PendingScrobble>,
    completed: Vec<String>,
    rejected: Vec<RejectedScrobble>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueResult {
    BelowThreshold,
    Duplicate,
    Queued,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    pub accepted: usize,
    pub rejected: usize,
    pub retried: usize,
}

pub struct ScrobbleQueue {
    path: PathBuf,
    state: State,
    // Дескриптор удерживает блокировку до уничтожения очереди.
    _lock: File,
}

impl ScrobbleQueue {
    /// Один файл на аккаунт Last.fm; ключи авторизации в файл не записываются.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let parent = directory(&path);
        std::fs::create_dir_all(parent)?;
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(PathBuf::from(lock_path))?;
        // Любая ошибка захвата означает занятость очереди: на Linux это
        // WouldBlock, на Windows — другой код той же коллизии блокировок.
        lock.try_lock_exclusive().map_err(|_| Error::QueueLocked)?;
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<State>(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: 1,
                ..State::default()
            },
            Err(e) => return Err(e.into()),
        };
        if state.version != 1 {
            return Err(Error::Invalid("unsupported queue version".into()));
        }
        Ok(Self {
            path,
            state,
            _lock: lock,
        })
    }
    pub fn pending(&self) -> &[PendingScrobble] {
        &self.state.pending
    }
    pub fn rejected(&self) -> &[RejectedScrobble] {
        &self.state.rejected
    }

    fn commit(&mut self, state: State) -> Result<()> {
        let parent = directory(&self.path);
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut tmp, &state)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|e| Error::Io(e.error))?;
        // После rename память обязана соответствовать уже записанному снимку.
        self.state = state;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn enqueue(
        &mut self,
        track: ScrobbleTrack,
        played_ms: u64,
        now_unix: u64,
    ) -> Result<EnqueueResult> {
        track.validate()?;
        if !track.eligible(played_ms) {
            return Ok(EnqueueResult::BelowThreshold);
        }
        if track.started_at_unix > now_unix {
            return Err(Error::Invalid("scrobble start is in the future".into()));
        }
        // Длительность и альбом не меняют идентичность одного прослушивания.
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &track.artist,
                &track.title,
                track.started_at_unix
            ))?)
        );
        if self.state.pending.iter().any(|e| e.key == key)
            || self.state.completed.contains(&key)
            || self.state.rejected.iter().any(|e| e.entry.key == key)
        {
            return Ok(EnqueueResult::Duplicate);
        }
        let mut state = self.state.clone();
        state.pending.push(PendingScrobble {
            key,
            track,
            attempts: 0,
            next_attempt_unix: now_unix,
        });
        self.commit(state)?;
        Ok(EnqueueResult::Queued)
    }

    /// Явный запуск, ограниченное число отправок. Фоновых задач нет.
    /// Отмена после ответа сервиса, но до commit допускает повторную доставку.
    pub async fn flush<S: ScrobbleSink + ?Sized>(
        &mut self,
        sink: &S,
        now_unix: u64,
        limit: usize,
    ) -> Result<FlushReport> {
        let keys: Vec<_> = self
            .state
            .pending
            .iter()
            .filter(|e| e.next_attempt_unix <= now_unix)
            .take(limit)
            .map(|e| e.key.clone())
            .collect();
        let mut report = FlushReport::default();
        for key in keys {
            let index = self
                .state
                .pending
                .iter()
                .position(|e| e.key == key)
                .expect("pending key");
            let result = sink.submit(&self.state.pending[index].track).await;
            let mut state = self.state.clone();
            let mut stop = false;
            match result {
                Ok(Submission::Accepted) => {
                    state.completed.push(state.pending.remove(index).key);
                    report.accepted += 1;
                }
                Ok(Submission::Ignored { code, message }) => {
                    // Лимит за сутки (5) и время из будущего (4) могут разрешиться позже.
                    if matches!(code, 4 | 5) {
                        retry(
                            &mut state.pending[index],
                            now_unix,
                            Some(if code == 5 { 86400 } else { 300 }),
                        );
                        report.retried += 1;
                        stop = true;
                    } else {
                        let entry = state.pending.remove(index);
                        state.rejected.push(RejectedScrobble {
                            entry,
                            reason: format!("ignored {code}: {message}"),
                        });
                        report.rejected += 1;
                    }
                }
                Err(error) if retryable(&error) => {
                    let delay = match &error {
                        Error::Http { retry_after, .. } => retry_after.map(|d| d.as_secs()),
                        _ => None,
                    };
                    retry(&mut state.pending[index], now_unix, delay);
                    report.retried += 1;
                    // Ошибка соединения, лимит или авторизация затрагивают весь аккаунт.
                    stop = true;
                }
                Err(error) => {
                    let entry = state.pending.remove(index);
                    state.rejected.push(RejectedScrobble {
                        entry,
                        reason: error.to_string(),
                    });
                    report.rejected += 1;
                }
            }
            self.commit(state)?;
            if stop {
                break;
            }
        }
        Ok(report)
    }

    /// Повтор после ручного исправления причины отказа; дедупликация сохраняется.
    pub fn retry_rejected(&mut self, key: &str, now_unix: u64) -> Result<bool> {
        let Some(index) = self.state.rejected.iter().position(|e| e.entry.key == key) else {
            return Ok(false);
        };
        let mut state = self.state.clone();
        let mut entry = state.rejected.remove(index).entry;
        entry.next_attempt_unix = now_unix;
        state.pending.push(entry);
        self.commit(state)?;
        Ok(true)
    }
}

fn directory(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn retry(entry: &mut PendingScrobble, now: u64, retry_after: Option<u64>) {
    entry.attempts = entry.attempts.saturating_add(1);
    let backoff = 30u64
        .saturating_mul(1u64 << entry.attempts.saturating_sub(1).min(12))
        .min(86400);
    entry.next_attempt_unix = now.saturating_add(backoff.max(retry_after.unwrap_or(0)));
}
fn retryable(error: &Error) -> bool {
    match error {
        Error::Transport(_) | Error::Protocol(_) | Error::Json(_) => true,
        Error::Http { status, .. } => matches!(status, 401 | 403 | 408 | 429 | 500..=599),
        // Ошибки ключей сохраняем: вызывающая сторона может заменить credentials.
        Error::Service { code, .. } => matches!(code, 4 | 8 | 9 | 10 | 11 | 13 | 16 | 26 | 29),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    struct Sink(bool);
    #[async_trait]
    impl ScrobbleSink for Sink {
        async fn submit(&self, _: &ScrobbleTrack) -> Result<Submission> {
            if self.0 {
                Ok(Submission::Accepted)
            } else {
                Err(Error::Http {
                    status: 429,
                    retry_after: Some(std::time::Duration::from_secs(120)),
                })
            }
        }
    }
    fn track() -> ScrobbleTrack {
        ScrobbleTrack {
            artist: "Артист".into(),
            title: "Трек".into(),
            album: None,
            duration_ms: 200_001,
            started_at_unix: 100,
        }
    }
    #[tokio::test]
    async fn threshold_retry_restart_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut queue = ScrobbleQueue::open(&path).unwrap();
        assert_eq!(
            queue.enqueue(track(), 100_000, 300).unwrap(),
            EnqueueResult::BelowThreshold
        );
        assert_eq!(
            queue.enqueue(track(), 100_001, 300).unwrap(),
            EnqueueResult::Queued
        );
        assert!(matches!(
            ScrobbleQueue::open(&path),
            Err(Error::QueueLocked)
        ));
        assert_eq!(queue.flush(&Sink(false), 300, 10).await.unwrap().retried, 1);
        assert_eq!(queue.pending()[0].next_attempt_unix, 420);
        drop(queue);
        let mut queue = ScrobbleQueue::open(&path).unwrap();
        assert_eq!(queue.pending()[0].track.started_at_unix, 100);
        assert_eq!(queue.flush(&Sink(true), 419, 10).await.unwrap().accepted, 0);
        assert_eq!(queue.flush(&Sink(true), 420, 10).await.unwrap().accepted, 1);
        drop(queue);
        let mut queue = ScrobbleQueue::open(&path).unwrap();
        assert_eq!(
            queue.enqueue(track(), 200_001, 500).unwrap(),
            EnqueueResult::Duplicate
        );
    }
    #[test]
    fn short_tracks_and_no_four_minute_shortcut() {
        let mut t = track();
        t.duration_ms = 30_000;
        assert!(!t.eligible(30_000));
        t.duration_ms = 1_200_000;
        assert!(!t.eligible(240_000));
        assert!(t.eligible(600_000));
    }
    #[test]
    fn corrupt_file_is_not_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        std::fs::write(&path, b"broken").unwrap();
        assert!(ScrobbleQueue::open(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"broken");
    }
}
