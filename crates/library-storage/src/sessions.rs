//! Локальное хранение сессий и статистики. Схема прежней SQLite сохраняется.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};
use uuid::Uuid;
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
const DAY_MS: u64 = 86_400_000;

#[derive(Clone)]
pub struct Store(SqlitePool);

impl Store {
    pub async fn record(
        &self,
        owner: &str,
        event: &str,
        track: &str,
        title: &str,
        artist: &str,
        milliseconds: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO listens(owner,event,track,title,artist,timestamp,milliseconds) VALUES(?,?,?,?,?,?,?) ON CONFLICT(owner,event) DO NOTHING")
            .bind(owner).bind(event).bind(track).bind(title).bind(artist)
            .bind(now_ms() as i64).bind(milliseconds as i64).execute(&self.0).await?;
        Ok(())
    }

    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .context("Не удалось открыть SQLite PUMP")?;
        sqlx::raw_sql(
            "
            CREATE TABLE IF NOT EXISTS sessions (
              token_hash TEXT PRIMARY KEY, owner TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS listens (
              owner TEXT NOT NULL, event TEXT NOT NULL, track TEXT NOT NULL,
              title TEXT NOT NULL, artist TEXT NOT NULL, timestamp INTEGER NOT NULL,
              milliseconds INTEGER NOT NULL CHECK(milliseconds BETWEEN 100 AND 30000),
              PRIMARY KEY(owner,event)
            );
            CREATE INDEX IF NOT EXISTS listens_period ON listens(owner, timestamp);
            CREATE INDEX IF NOT EXISTS listens_ts ON listens(timestamp);
        ",
        )
        .execute(&pool)
        .await?;
        // Статистика читается только за 7/30 дней: события старше 35 дней
        // удаляются при каждом открытии, база не растёт бесконечно.
        // Сессии при этом не трогаются — токены остаются действительными.
        let cutoff = now_ms().saturating_sub(35 * DAY_MS) as i64;
        sqlx::query("DELETE FROM listens WHERE timestamp < ?")
            .bind(cutoff)
            .execute(&pool)
            .await?;
        Ok(Self(pool))
    }

    pub async fn owner(&self, token: &str) -> Result<Option<String>, sqlx::Error> {
        if !valid_token(token) {
            return Ok(None);
        }
        sqlx::query_scalar("SELECT owner FROM sessions WHERE token_hash = ?")
            .bind(hash(token))
            .fetch_optional(&self.0)
            .await
    }

    pub async fn new_session(&self, owner: &str) -> Result<String, sqlx::Error> {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO sessions(token_hash, owner) VALUES(?,?)")
            .bind(hash(&token))
            .bind(owner)
            .execute(&self.0)
            .await?;
        Ok(token)
    }

    pub async fn read(&self, owner: &str, period: Period) -> Result<Summary, sqlx::Error> {
        let time = now_ms().saturating_sub(DAY_MS * period.days()) as i64;
        let since = |statement| sqlx::query(statement).bind(owner).bind(time);
        let totals = since("SELECT COALESCE(SUM(milliseconds),0) AS ms, COUNT(DISTINCT track) AS tracks FROM listens WHERE owner=? AND timestamp>=?")
            .fetch_one(&self.0).await?;
        let rows = since("SELECT MAX(title) AS name, MAX(artist) AS artist, SUM(milliseconds) AS ms FROM listens WHERE owner=? AND timestamp>=? GROUP BY track ORDER BY ms DESC, name LIMIT 5")
            .fetch_all(&self.0).await?;
        let artists = since("SELECT artist AS name, SUM(milliseconds) AS ms FROM listens WHERE owner=? AND timestamp>=? AND artist<>'' GROUP BY artist ORDER BY ms DESC, name LIMIT 5")
            .fetch_all(&self.0).await?;
        Ok(Summary {
            period,
            milliseconds: totals.get("ms"),
            tracks: totals.get("tracks"),
            top_tracks: rows
                .into_iter()
                .map(|row| Rank {
                    title: row.get("name"),
                    artist: row.get("artist"),
                    milliseconds: row.get("ms"),
                })
                .collect(),
            top_artists: artists
                .into_iter()
                .map(|row| Rank {
                    title: row.get("name"),
                    artist: String::new(),
                    milliseconds: row.get("ms"),
                })
                .collect(),
        })
    }
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
pub fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    #[default]
    Week,
    Month,
}
impl Period {
    fn days(self) -> u64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
        }
    }
}

#[derive(Serialize)]
pub struct Summary {
    pub period: Period,
    pub milliseconds: i64,
    pub tracks: i64,
    pub top_tracks: Vec<Rank>,
    pub top_artists: Vec<Rank>,
}

#[derive(Serialize)]
pub struct Rank {
    pub title: String,
    pub artist: String,
    pub milliseconds: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retention_keeps_month_and_drops_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pump.sqlite");
        let store = Store::open(&path).await.unwrap();
        let old = now_ms().saturating_sub(40 * DAY_MS) as i64;
        sqlx::query(
            "INSERT INTO listens(owner,event,track,title,artist,timestamp,milliseconds) VALUES(?,?,?,?,?,?,?)",
        )
        .bind("guest:x")
        .bind("old-event")
        .bind("t")
        .bind("T")
        .bind("A")
        .bind(old)
        .bind(1000)
        .execute(&store.0)
        .await
        .unwrap();
        store
            .record("guest:x", "new-event", "t", "T", "A", 2000)
            .await
            .unwrap();
        store.0.close().await;
        drop(store);
        let store = Store::open(&path).await.unwrap();
        let summary = store.read("guest:x", Period::Month).await.unwrap();
        assert_eq!(summary.milliseconds, 2000);
        assert_eq!(summary.tracks, 1);
        store.0.close().await;
    }
}
