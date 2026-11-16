//! Connection prewarming: detached, best effort, and never a prerequisite.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Answer every request with an empty success so a `HEAD` counts as warm.
async fn serve(listener: TcpListener, hits: Arc<AtomicUsize>) {
    while let Ok((mut stream, _)) = listener.accept().await {
        hits.fetch_add(1, Ordering::SeqCst);
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request).await;
        let _ = stream
            .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        let _ = stream.shutdown().await;
    }
}

#[tokio::test]
async fn warm_opens_each_configured_origin_once() {
    let first = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("first listener");
    let second = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("second listener");
    let first_address = first.local_addr().expect("first address");
    let second_address = second.local_addr().expect("second address");
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first_server = tokio::spawn(serve(first, first_hits.clone()));
    let second_server = tokio::spawn(serve(second, second_hits.clone()));

    let targets = miao_engine::prewarm::targets([
        format!("http://{first_address}/v1/chat/completions"),
        // The same origin on another path is one connection, not two.
        format!("http://{first_address}/v1/responses"),
        format!("http://{second_address}/v1/messages"),
    ]);
    assert_eq!(targets.len(), 2);
    let warmed = tokio::time::timeout(
        Duration::from_secs(10),
        miao_engine::prewarm::warm(&targets),
    )
    .await
    .expect("prewarm finishes");

    assert_eq!(warmed, 2);
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn a_failed_warmup_is_silent_and_leaves_later_requests_alone() {
    // Nothing listens on this address: the warmup must report failure without
    // an error, a panic, or a retry loop.
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserved port");
    let dead = closed.local_addr().expect("dead address");
    drop(closed);
    let dead_targets = miao_engine::prewarm::targets([format!("http://{dead}/v1")]);
    assert_eq!(miao_engine::prewarm::warm(&dead_targets).await, 0);

    // A warmed, reachable endpoint keeps answering afterwards.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("live listener");
    let address = listener.local_addr().expect("live address");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve(listener, hits.clone()));

    let warmup = miao_engine::prewarm::spawn(miao_engine::prewarm::targets([format!(
        "http://{address}/v1"
    )]));
    tokio::time::timeout(Duration::from_secs(10), warmup)
        .await
        .expect("warmup task finishes")
        .expect("warmup task does not panic");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let response = reqwest::get(format!("http://{address}/health"))
        .await
        .expect("later request reaches the endpoint");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    server.abort();
}

#[tokio::test]
async fn a_silent_endpoint_does_not_block_the_spawning_path() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("silent listener");
    let address = listener.local_addr().expect("silent address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        // Hold the socket open without answering: only the warmup's own
        // timeout ends this, never the start path that spawned it.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(stream);
    });

    let started = std::time::Instant::now();
    let warmup = miao_engine::prewarm::spawn(miao_engine::prewarm::targets([format!(
        "http://{address}/v1"
    )]));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "spawning a warmup must not wait for the handshake"
    );
    drop(warmup);
    server.abort();
}

#[test]
fn targets_drop_values_that_cannot_be_warmed() {
    let targets = miao_engine::prewarm::targets([
        "https://api.example/v1".to_string(),
        "https://api.example/v2".to_string(),
        "ftp://api.example/v1".to_string(),
        "not a url".to_string(),
    ]);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].as_str(), "https://api.example/v1");
}

#[test]
fn loopback_endpoints_are_recognised() {
    let targets = miao_engine::prewarm::targets([
        "http://127.0.0.1:1234/v1".to_string(),
        "http://[::1]:1234/v1".to_string(),
        "http://localhost:1234/v1".to_string(),
        "http://192.0.2.10/v1".to_string(),
    ]);
    assert_eq!(targets.len(), 4);
    assert_eq!(
        targets
            .iter()
            .filter(|url| miao_engine::prewarm::is_loopback(url))
            .count(),
        3
    );
}
