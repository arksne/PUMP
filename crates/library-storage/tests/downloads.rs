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
