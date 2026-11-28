use miao_engine::store::Store;
use serde_json::json;

async fn admitted(store: &Store, session: &str) {
    store
        .admit(miao_engine::protocol::Input {
            agent_references: Default::default(),
            command: None,
            session_id: session.into(),
            input_id: format!("seed-{session}"),
            prompt: "task".into(),
            delivery: miao_engine::protocol::Delivery::Steer,
        })
        .await
        .unwrap();
}

fn codes(report: &serde_json::Value) -> Vec<String> {
    report["cacheDiagnostics"]["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["code"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn cache_health_names_what_is_missing_before_it_names_a_ratio() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    for session in ["healthy", "unreported", "cold", "invalid"] {
        admitted(&store, session).await;
    }

    // A route whose provider reports both the cached and the uncached input:
    // the ratio is measurable and there is nothing to recommend.
    store
        .record(
            "healthy",
            "usage",
            json!({"run_id":"r1","step":0,
                   "usage":{"inputTokens":200,"outputTokens":10,"cacheReadInputTokens":150,"nonCachedInputTokens":50}}),
        )
        .await
        .unwrap();
    // The provider reports input but never a cache breakdown, so the ratio is
    // unknown rather than zero.
    store
        .record(
            "unreported",
            "usage",
            json!({"run_id":"r1","step":0,"usage":{"inputTokens":200,"outputTokens":10}}),
        )
        .await
        .unwrap();
    // Every input token is uncached: caching is measurable and never hit.
    store
        .record(
            "cold",
            "usage",
            json!({"run_id":"r1","step":0,
                   "usage":{"inputTokens":200,"outputTokens":10,"cacheReadInputTokens":0,"nonCachedInputTokens":200}}),
        )
        .await
        .unwrap();
    // More cache tokens than input tokens cannot be a ratio at all.
    store
        .record(
            "invalid",
            "usage",
            json!({"run_id":"r1","step":0,
                   "usage":{"inputTokens":200,"outputTokens":10,"cacheReadInputTokens":300}}),
        )
        .await
        .unwrap();

    let healthy = store.usage_report("healthy").await.unwrap();
    assert_eq!(healthy["cacheDiagnostics"]["hitRatio"], json!(0.75));
    assert!(codes(&healthy).is_empty(), "{healthy}");

    let unreported = store.usage_report("unreported").await.unwrap();
    assert_eq!(unreported["cacheDiagnostics"]["hitRatio"], json!(null));
    assert_eq!(codes(&unreported), ["unreportedCacheReads"]);

    let cold = store.usage_report("cold").await.unwrap();
    assert_eq!(cold["cacheDiagnostics"]["hitRatio"], json!(0.0));
    assert_eq!(codes(&cold), ["noCacheReads"]);

    let invalid = store.usage_report("invalid").await.unwrap();
    assert_eq!(invalid["cacheDiagnostics"]["hitRatio"], json!(null));
    assert_eq!(codes(&invalid), ["invalidBreakdown"]);
    // The recommendation carries the count it is based on, not a generic hint.
    assert!(
        invalid["cacheDiagnostics"]["recommendations"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("1 usage report(s)"),
        "{invalid}"
    );
}

#[tokio::test]
async fn cache_misses_are_counted_by_the_prefix_that_moved() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    admitted(&store, "ledger").await;

    // Three settled turns: a hit, a miss inside the warm window, and the
    // rebuild a compaction asked for.
    store
        .record(
            "ledger",
            "usage",
            json!({"run_id":"r1","step":0,
                   "usage":{"inputTokens":4000,"outputTokens":10,"cacheReadInputTokens":3900},
                   "cache":{"read":3900,"hitRatio":0.975,"warm":true,"miss":false,"cause":"none","expectedRebuild":false}}),
        )
        .await
        .unwrap();
    store
        .record(
            "ledger",
            "usage",
            json!({"run_id":"r1","step":1,
                   "usage":{"inputTokens":4200,"outputTokens":10,"cacheReadInputTokens":0},
                   "cache":{"read":0,"warm":true,"miss":true,"cause":"prefix-change","expectedRebuild":false}}),
        )
        .await
        .unwrap();
    store
        .record(
            "ledger",
            "usage",
            json!({"run_id":"r1","step":2,
                   "usage":{"inputTokens":1000,"outputTokens":10,"cacheWriteInputTokens":1000},
                   "cache":{"read":0,"write":1000,"warm":false,"miss":true,"cause":"rebuild","expectedRebuild":true}}),
        )
        .await
        .unwrap();

    let report = store.usage_report("ledger").await.unwrap();
    let diagnostics = &report["cacheDiagnostics"];
    assert_eq!(diagnostics["misses"]["turns"], json!(2));
    assert_eq!(diagnostics["misses"]["byCause"]["none"], json!(1));
    assert_eq!(diagnostics["misses"]["byCause"]["prefix-change"], json!(1));
    assert_eq!(diagnostics["misses"]["byCause"]["rebuild"], json!(1));
    // Only the missing turns' prompts count: 4200 + 1000.
    assert_eq!(diagnostics["misses"]["tokens"], json!(5200));
    assert_eq!(diagnostics["misses"]["last"]["cause"], json!("rebuild"));
    assert_eq!(diagnostics["misses"]["last"]["tokens"], json!(1000));
    assert_eq!(diagnostics["expectedRebuilds"], json!(1));
    assert_eq!(diagnostics["warmTurns"], json!(2));
    assert!(
        codes(&report).contains(&"expectedRebuilds".to_string()),
        "{report}"
    );
}
