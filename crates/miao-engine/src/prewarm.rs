//! Best-effort connection prewarming for the configured provider endpoints.
//!
//! A start whose provider lives behind the network pays DNS, TCP and TLS setup
//! on its first request. Doing that once in the background after startup moves
//! the cost off the first admission's critical path. A warmup never carries a
//! model request and never authenticates: a `HEAD` is answered with 401, 403 or
//! 404 by every real endpoint, and any answer proves the socket, the TLS
//! handshake and the HTTP connection are ready.
//!
//! Nothing may depend on a warmup. It is spawned detached, it has a bounded
//! timeout, and every failure (offline, no credential, a proxy that refuses,
//! an endpoint that does not speak HTTP) is swallowed: a start and a run behave
//! exactly as they would without it.

use std::time::Duration;

/// One warmup attempt may not outlive this. Long enough for a slow DNS/TLS
/// handshake, short enough that an abandoned attempt is gone before a human
/// notices the first turn.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Turn configured endpoint strings into warmup targets.
///
/// Only `http`/`https` origins can be warmed, and one target per origin is
/// enough: connection reuse is per scheme, host and port, so several model
/// paths on the same gateway do not need several warmups. Malformed values are
/// dropped rather than refused, because a warmup must never decide whether a
/// start succeeds.
pub fn targets(values: impl IntoIterator<Item = String>) -> Vec<reqwest::Url> {
    let mut origins = std::collections::BTreeSet::new();
    values
        .into_iter()
        .filter_map(|value| reqwest::Url::parse(value.trim()).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .filter(|url| {
            origins.insert((
                url.scheme().to_owned(),
                url.host_str().unwrap_or_default().to_owned(),
                url.port_or_known_default(),
            ))
        })
        .collect()
}

/// A loopback endpoint completes its handshake in microseconds, and a local
/// fixture must never see traffic it did not ask for. Callers skip these.
pub fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            // `host_str` keeps an IPv6 literal's brackets.
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

/// Start the warmup in the background and return its handle.
///
/// The handle is only for tests and shutdown paths; a production start drops it
/// immediately so the task can never gate startup, a turn, or exit.
pub fn spawn(targets: Vec<reqwest::Url>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        warm(&targets).await;
    })
}

/// Warm every target and report how many connections answered.
///
/// The count exists for tests and diagnostics; callers in the start path ignore
/// it. Concurrent by construction: independent endpoints must not wait on each
/// other, and one unreachable endpoint must not delay the rest.
pub async fn warm(targets: &[reqwest::Url]) -> usize {
    if targets.is_empty() {
        return 0;
    }
    let Ok(client) = crate::provider::http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TIMEOUT)
        .timeout(TIMEOUT)
        .build()
    else {
        return 0;
    };
    futures_util::future::join_all(targets.iter().map(|url| warm_one(&client, url)))
        .await
        .into_iter()
        .filter(|ready| *ready)
        .count()
}

async fn warm_one(client: &reqwest::Client, url: &reqwest::Url) -> bool {
    client.head(url.as_str()).send().await.is_ok()
}
