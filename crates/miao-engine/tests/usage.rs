use miao_engine::pricing::{Table, Tokens};
use miao_engine::store::Store;
use miao_engine::usage::{anthropic, bedrock, chat, gemini, responses};
use serde_json::json;

#[test]
fn anthropic_reports_the_breakdown_and_sums_the_inclusive_total() {
    let usage = anthropic(
        &json!({"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}),
    );
    assert_eq!(usage["nonCachedInputTokens"], 10);
    assert_eq!(usage["cacheReadInputTokens"], 90);
    assert_eq!(usage["cacheWriteInputTokens"], 5);
    assert_eq!(usage["inputTokens"], 105);
    assert_eq!(usage["outputTokens"], 4);
    assert_eq!(usage["totalTokens"], 109);
    // Extended thinking is billed inside output_tokens; never guessed here.
    assert!(usage.get("reasoningTokens").is_none());
    assert_eq!(
        usage["providerMetadata"]["anthropic-messages"]["input_tokens"],
        10
    );
    assert!(anthropic(&json!({})).get("providerMetadata").is_none());
}

/// Bedrock's Converse usage carries cache reads and writes, so the cache
/// creation has its own bucket; it has no reasoning counter, so thinking stays
/// unknown instead of being guessed from the output count.
#[test]
fn bedrock_separates_cache_writes_and_leaves_reasoning_unknown() {
    let usage = bedrock(
        &json!({"inputTokens":100,"outputTokens":9,"cacheReadInputTokens":60,"cacheWriteInputTokens":25}),
    );
    assert_eq!(usage["inputTokens"], 100);
    assert_eq!(usage["nonCachedInputTokens"], 15);
    assert_eq!(usage["cacheReadInputTokens"], 60);
    assert_eq!(usage["cacheWriteInputTokens"], 25);
    assert_eq!(usage["totalTokens"], 109);
    assert!(usage.get("reasoningTokens").is_none());
}

#[test]
fn chat_covers_openai_and_deepseek_cache_reporting() {
    let openai = chat(
        &json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":60},"completion_tokens_details":{"reasoning_tokens":7}}),
    );
    assert_eq!(openai["inputTokens"], 100);
    assert_eq!(openai["cacheReadInputTokens"], 60);
    assert_eq!(openai["nonCachedInputTokens"], 40);
    assert_eq!(openai["reasoningTokens"], 7);
    assert_eq!(openai["totalTokens"], 120);
    // Chat Completions reports no cache-creation counter; inventing one would
    // bill a write the protocol never charged.
    assert!(openai.get("cacheWriteInputTokens").is_none());
    let deepseek = chat(
        &json!({"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}),
    );
    assert_eq!(deepseek["cacheReadInputTokens"], 80);
    assert_eq!(deepseek["nonCachedInputTokens"], 20);
    assert_eq!(deepseek["totalTokens"], 120);
}

#[test]
fn responses_and_gemini_keep_the_inclusive_output_contract() {
    let responses_usage = responses(
        &json!({"input_tokens":50,"output_tokens":9,"input_tokens_details":{"cached_tokens":30},"output_tokens_details":{"reasoning_tokens":4}}),
    );
    assert_eq!(responses_usage["nonCachedInputTokens"], 20);
    assert_eq!(responses_usage["reasoningTokens"], 4);
    assert_eq!(responses_usage["totalTokens"], 59);
    // Gemini reports visible candidates only, so output must include thinking.
    let gemini_usage = gemini(
        &json!({"promptTokenCount":40,"candidatesTokenCount":6,"thoughtsTokenCount":4,"cachedContentTokenCount":10,"totalTokenCount":50}),
    );
    assert_eq!(gemini_usage["inputTokens"], 40);
    assert_eq!(gemini_usage["nonCachedInputTokens"], 30);
    assert_eq!(gemini_usage["cacheReadInputTokens"], 10);
    assert_eq!(gemini_usage["outputTokens"], 10);
    assert_eq!(gemini_usage["reasoningTokens"], 4);
    // Gemini's implicit cache reports reads only; a write bucket would be
    // fabricated, so it stays absent.
    assert!(gemini_usage.get("cacheWriteInputTokens").is_none());
    assert_eq!(gemini_usage["totalTokens"], 50);
}

#[test]
fn responses_separates_cache_writes_from_ordinary_input_and_preserves_raw_usage() {
    let raw = json!({"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":25}});
    let usage = responses(&raw);
    assert_eq!(usage["inputTokens"], 100);
    assert_eq!(usage["cacheReadInputTokens"], 60);
    assert_eq!(usage["cacheWriteInputTokens"], 25);
    assert_eq!(usage["nonCachedInputTokens"], 15);
    assert_eq!(usage["totalTokens"], 107);
    assert_eq!(usage["providerMetadata"]["openai-responses"], raw);
    assert_eq!(miao_engine::usage::cache_lineage(&usage)["write"], 25);
}

#[test]
fn responses_cache_write_counts_remain_optional_and_invalid_breakdowns_are_bounded() {
    let legacy = responses(&json!({"input_tokens":50,"input_tokens_details":{"cached_tokens":30}}));
    assert_eq!(legacy["nonCachedInputTokens"], 20);
    assert!(legacy.get("cacheWriteInputTokens").is_none());
    let missing = responses(&json!({}));
    assert!(missing.get("nonCachedInputTokens").is_none());
    assert!(missing.get("cacheWriteInputTokens").is_none());
    let zero = responses(
        &json!({"input_tokens":50,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":0}}),
    );
    assert_eq!(zero["nonCachedInputTokens"], 20);
    assert_eq!(zero["cacheWriteInputTokens"], 0);
    let invalid = responses(
        &json!({"input_tokens":50,"input_tokens_details":{"cached_tokens":40,"cache_write_tokens":25}}),
    );
    assert_eq!(invalid["nonCachedInputTokens"], 0);
    assert_eq!(invalid["cacheWriteInputTokens"], 25);
}

/// One provider turn as it reaches the ledger: the adapter's raw payload
/// normalized, priced, and recorded as a `usage` event. Cache creation and
/// reasoning have to survive as their own buckets all the way to the report,
/// and the money has to bill cache creation at its own rate instead of as
/// ordinary input or as a cache read.
#[tokio::test]
async fn cache_creation_and_reasoning_reach_the_usage_event_and_its_cost() {
    let usage = responses(&json!({
        "input_tokens": 1_000_000,
        "output_tokens": 200_000,
        "input_tokens_details": {"cached_tokens": 500_000, "cache_write_tokens": 100_000},
        "output_tokens_details": {"reasoning_tokens": 150_000},
    }));
    let table = Table::from_config(&json!({
        "providers": {"p": {"models": {"m": {
            "currency": "USD",
            "cost": {"input": 1.0, "output": 10.0, "cache_read": 0.1, "cache_write": 1.25},
        }}}},
    }));
    let charge = table.charge("p/m", Tokens::from_usage(&usage)).unwrap();
    // 0.4M × 1.0 + 0.2M × 10.0 + 0.5M × 0.1 + 0.1M × 1.25 = 2.575. Reasoning
    // is a subset of output, so it adds nothing beside it.
    assert_eq!(charge.amount, 2.575);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    store.ensure("s").await.unwrap();
    store
        .record(
            "s",
            "usage",
            json!({
                "run_id": "run",
                "step": 0,
                "route": "p/m",
                "usage": usage,
                "cost": {"amount": charge.amount, "currency": charge.currency},
            }),
        )
        .await
        .unwrap();
    let report = store.usage_report("s").await.unwrap();
    assert_eq!(report["tokens"]["cacheWrite"], 100_000);
    assert_eq!(report["tokens"]["reasoning"], 150_000);
    assert_eq!(report["providerUsage"]["tokens"]["cacheWrite"], 100_000);
    assert_eq!(report["providerUsage"]["tokens"]["reasoning"], 150_000);
    assert_eq!(report["cost"]["byCurrency"][0]["currency"], "USD");
    assert_eq!(report["cost"]["byCurrency"][0]["amount"], 2.575);
}
