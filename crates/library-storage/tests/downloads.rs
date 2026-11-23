//! Перезапуск и взаимное исключение менеджеров защищают каталог от потери заданий.
use library_storage::{
    DownloadManager, DownloadRequest, NetworkMode, StorageConfig, TrackMetadata,
};
#[tokio::test]
async fn queue_survives_restart_and_excludes_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        root: dir.path().into(),
        concurrency: 1,
        max_bytes: 1024,
        mode: NetworkMode::Online,
    };
    let manager = DownloadManager::open(config.clone()).await.unwrap();
    assert!(DownloadManager::open(config.clone()).await.is_err());
    manager
        .enqueue(DownloadRequest {
            id: "track".into(),
            url: "https://example.com/music.mp3".parse().unwrap(),
            extension: "mp3".into(),
            cover_url: None,
            metadata: TrackMetadata::default(),
        })
        .await
        .unwrap();
    drop(manager);
    let manager = DownloadManager::open(config).await.unwrap();
    assert_eq!(manager.jobs().await[0].request.id, "track");
}
#[tokio::test]
async fn local_mode_blocks_network_before_creating_job() {
    let dir = tempfile::tempdir().unwrap();
    let manager = DownloadManager::open(StorageConfig {
        root: dir.path().into(),
        concurrency: 1,
        max_bytes: 1024,
        mode: NetworkMode::LocalOnly,
    })
    .await
    .unwrap();
    assert!(
        manager
            .enqueue(DownloadRequest {
                id: "track".into(),
                url: "https://example.com/music.mp3".parse().unwrap(),
                extension: "mp3".into(),
                cover_url: None,
                metadata: TrackMetadata::default()
            })
            .await
            .is_err()
    );
    assert!(manager.jobs().await.is_empty());
}

#[tokio::test]
async fn downloads_audio_and_writes_metadata_atomically() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut wav = Vec::new();
    let payload = vec![0u8; 1600];
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36u32 + payload.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8000u32.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    wav.extend(payload);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut input = [0; 4096];
        socket.read(&mut input).await.unwrap();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            wav.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(&wav).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let manager = DownloadManager::open(StorageConfig {
        root: dir.path().into(),
        concurrency: 1,
        max_bytes: 10000,
        mode: NetworkMode::Online,
    })
    .await
    .unwrap();
    manager
        .enqueue(DownloadRequest {
            id: "wav".into(),
            url: format!("http://{address}/track.wav").parse().unwrap(),
            extension: "wav".into(),
            cover_url: None,
            metadata: TrackMetadata {
                title: Some("Downloaded track".into()),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let track = manager
        .run("wav", library_storage::CancellationToken::new())
        .await
        .unwrap();
    assert!(track.audio_path.exists());
    assert_eq!(
        library_storage::tags::read_tags(&track.audio_path)
            .unwrap()
            .metadata
            .title
            .as_deref(),
        Some("Downloaded track")
    );
    assert_eq!(
        manager.jobs().await[0].status,
        library_storage::JobStatus::Completed
    );
    server.await.unwrap();
}
