//! Opt-in contracts against real Valkey servers; `scripts/with-valkey.sh` starts them.
use codeyard_cache::{Cache, Config, Family, Mode};
use std::{sync::Arc, time::Duration};

async fn contract(variable: &str, mode: Mode) {
    let Ok(urls) = std::env::var(variable) else {
        eprintln!("SKIP Valkey contract: {variable} is absent; this is not a remote-cache pass");
        return;
    };
    assert!(!urls.trim().is_empty(), "{variable} must not be empty");
    let namespace = format!(
        "contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let config = || {
        let mut config = Config::local(&namespace, 7);
        config.mode = mode;
        config.urls = urls.split(',').map(str::to_owned).collect();
        config
    };
    let writer = Cache::new(config()).unwrap();
    let reader = Cache::new(config()).unwrap();
    let keys: Vec<_> = (0..8)
        .map(|index| writer.key(Family::Markdown, &[format!("document-{index}").as_bytes()]))
        .collect();
    for (index, key) in keys.iter().enumerate().take(7) {
        writer
            .put(
                key,
                Arc::from(format!("rendered-{index}").into_bytes()),
                Duration::from_secs(10),
            )
            .await;
    }
    // A fresh process tier proves these are remote hits, not writes to the local LRU.
    let values = reader.mget(&keys).await.unwrap();
    for (index, value) in values.iter().enumerate().take(7) {
        assert_eq!(
            value.as_deref(),
            Some(format!("rendered-{index}").as_bytes())
        );
    }
    assert!(values[7].is_none());
    let reordered = reader
        .mget(&[
            keys[6].clone(),
            keys[7].clone(),
            keys[0].clone(),
            keys[6].clone(),
        ])
        .await
        .unwrap();
    assert_eq!(reordered[0], reordered[3]);
    assert!(reordered[1].is_none());
    assert_eq!(reordered[2].as_deref(), Some(&b"rendered-0"[..]));
    writer.delete(&keys).await.unwrap();
    let empty = Cache::new(config()).unwrap();
    assert!(empty.mget(&keys).await.unwrap().iter().all(Option::is_none));
    let expiring = writer.key(Family::Diff, &[b"expiring"]);
    writer
        .put(
            &expiring,
            Arc::from(&b"short lived"[..]),
            Duration::from_millis(100),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(empty.get(&expiring).await.is_none());
    assert!(writer.get(&expiring).await.is_none());
}

#[tokio::test]
async fn standalone_contract() {
    contract("CODEYARD_TEST_VALKEY_URL", Mode::Standalone).await;
}
#[tokio::test]
async fn cluster_contract() {
    contract("CODEYARD_TEST_VALKEY_CLUSTER_URLS", Mode::Cluster).await;
}
