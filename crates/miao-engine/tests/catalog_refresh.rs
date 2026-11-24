//! The fetched catalog: a 200 replaces the cache, a 304 or a fresh cache
//! revalidates without a write, and a body that is not a catalog never replaces
//! the snapshot a machine already has.

use miao_engine::model_catalog::{refresh_from, Refreshed};
use std::io::{Read, Write};

const CATALOG: &str = r#"{"alpha":{"models":{"m1":{},"m2":{}}},"beta":{"models":{"m3":{}}}}"#;

fn http(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
}

/// Serve the scripted responses in order, one connection each, and hand back
/// the request head of each so a test can read the conditional request.
fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            requests.push(String::from_utf8_lossy(&buffer[..read]).to_lowercase());
            stream.write_all(response.as_bytes()).expect("write");
            stream.flush().expect("flush");
        }
        requests
    });
    (format!("http://{address}/models/api.json"), handle)
}

fn cache(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().join("miao").join("models.json")
}

#[tokio::test]
async fn a_fetched_catalog_replaces_the_cache_and_records_its_etag() {
    let (url, server) = serve(vec![http("200 OK", "etag: \"v1\"\r\n", CATALOG)]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cache(&directory);
    let outcome = refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await;
    assert_eq!(
        outcome,
        Refreshed::Updated {
            providers: 2,
            models: 3
        }
    );
    assert_eq!(std::fs::read_to_string(&path).expect("cache"), CATALOG);
    let meta =
        std::fs::read_to_string(path.with_file_name("models.json.meta.json")).expect("sidecar");
    let meta: serde_json::Value = serde_json::from_str(&meta).expect("sidecar JSON");
    assert_eq!(meta["etag"], serde_json::json!("\"v1\""));
    server.join().expect("server");
}

#[tokio::test]
async fn a_304_revalidates_without_rewriting_the_cache() {
    let (url, server) = serve(vec![
        http("200 OK", "etag: \"v1\"\r\n", CATALOG),
        "HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n".to_string(),
    ]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cache(&directory);
    let first = refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await;
    assert!(matches!(first, Refreshed::Updated { .. }), "{first:?}");
    // A second forced refresh revalidates instead of downloading again.
    let second = refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await;
    assert_eq!(second, Refreshed::Current);
    assert_eq!(std::fs::read_to_string(&path).expect("cache"), CATALOG);
    let requests = server.join().expect("server");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].contains("if-none-match: \"v1\""),
        "the second request revalidates: {}",
        requests[1]
    );
}

#[tokio::test]
async fn a_catalog_inside_its_ttl_is_not_fetched_again() {
    let (url, server) = serve(vec![http("200 OK", "", CATALOG)]);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cache(&directory);
    refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await;
    server.join().expect("server");
    // The listener is gone: a second request would fail, so `Current` proves
    // the TTL answered it from the sidecar.
    let outcome = refresh_from(&url, &path, false, std::time::Duration::from_secs(3600)).await;
    assert_eq!(outcome, Refreshed::Current);
}

#[tokio::test]
async fn a_body_that_is_not_a_catalog_keeps_the_snapshot_already_on_disk() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cache(&directory);
    let (url, server) = serve(vec![
        http("200 OK", "", CATALOG),
        http("200 OK", "", "<html>blocked by a proxy</html>"),
        http("200 OK", "", r#"{"alpha":{"id":"alpha"}}"#),
        http("503 Service Unavailable", "", ""),
    ]);
    assert!(matches!(
        refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await,
        Refreshed::Updated { .. }
    ));
    for _ in 0..3 {
        let outcome = refresh_from(&url, &path, true, std::time::Duration::from_secs(60)).await;
        assert!(matches!(outcome, Refreshed::Unavailable(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("cache"),
            CATALOG,
            "a rejected body must not replace the catalog"
        );
    }
    server.join().expect("server");
}

#[tokio::test]
async fn an_unreachable_source_keeps_the_snapshot_already_on_disk() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = cache(&directory);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, CATALOG).expect("write");
    // Port 1 is closed on every platform the tests run on; the refresh reports
    // it and leaves the file alone.
    let outcome = refresh_from(
        "http://127.0.0.1:1/models/api.json",
        &path,
        true,
        std::time::Duration::from_secs(60),
    )
    .await;
    assert!(matches!(outcome, Refreshed::Unavailable(_)), "{outcome:?}");
    assert_eq!(std::fs::read_to_string(&path).expect("cache"), CATALOG);
}
