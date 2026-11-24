//! Does the self-maintained overlay still cover a gap? The overlay exists for
//! models and fields the served catalog does not carry yet, so an entry the
//! catalog has caught up with is dead weight that hides a real difference the
//! next time someone reads the table.
//!
//! This reaches the live catalog, so it is `#[ignore]`d and the `catalog-drift`
//! workflow runs it on a schedule (and on demand) instead of on every push.

use std::time::Duration;

/// Where the site serves the catalog the product reads.
const DEFAULT_URL: &str = "https://mtty.dev/models/api.json";
const MAX_BODY: usize = 64 * 1024 * 1024;

#[tokio::test]
#[ignore = "reaches mtty.dev; the catalog-drift workflow runs it on a schedule"]
async fn every_overlay_entry_still_covers_a_gap_in_the_served_catalog() {
    let url = std::env::var("MIAO_MODELS_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let body = reqwest::Client::builder()
        .user_agent("miao-catalog-drift")
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client")
        .get(&url)
        .send()
        .await
        .expect("fetch the catalog")
        .error_for_status()
        .expect("the catalog answers")
        .bytes()
        .await
        .expect("catalog body");
    assert!(body.len() <= MAX_BODY, "{} bytes", body.len());
    let catalog: serde_json::Value = serde_json::from_slice(&body).expect("catalog JSON");
    let providers = catalog.as_object().expect("catalog object");
    assert!(
        providers.len() > 100,
        "only {} providers: the URL answered something that is not the catalog",
        providers.len()
    );

    let patches = miao_engine::model_catalog::overlay_patches();
    let mut gone = Vec::new();
    let mut redundant = Vec::new();
    let mut review = Vec::new();
    let mut additions = 0;
    for (provider_id, models) in patches.as_object().expect("patch table") {
        let Some(upstream) = providers
            .get(provider_id)
            .and_then(|provider| provider.get("models"))
            .and_then(serde_json::Value::as_object)
        else {
            gone.push(provider_id.clone());
            continue;
        };
        for (model_id, patch) in models.as_object().expect("model patches") {
            let Some(entry) = upstream
                .get(model_id)
                .and_then(serde_json::Value::as_object)
            else {
                // The catalog has not caught up: the overlay is still what
                // makes this model usable.
                additions += 1;
                println!(
                    "{provider_id}/{model_id}: the catalog does not list it; the overlay adds it"
                );
                continue;
            };
            let fields = patch.as_object().expect("fields");
            let equal: Vec<&String> = fields
                .iter()
                .filter(|(field, value)| entry.get(field.as_str()) == Some(value))
                .map(|(field, _)| field)
                .collect();
            if equal.len() == fields.len() {
                redundant.push(format!(
                    "{provider_id}/{model_id}: the catalog already carries {} — delete the overlay entry",
                    fields.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
            let kept: Vec<&String> = fields
                .keys()
                .filter(|field| !equal.contains(field))
                .collect();
            let kept = kept.iter().map(|field| field.as_str()).collect::<Vec<_>>();
            println!(
                "{provider_id}/{model_id}: the catalog lists it, but still differs on {}",
                kept.join(", ")
            );
            if equal.is_empty() {
                continue;
            }
            review.push(format!(
                "{provider_id}/{model_id}: {} now match the catalog and can be dropped from the entry",
                equal
                    .iter()
                    .map(|field| field.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for line in &review {
        println!("REVIEW {line}");
    }
    assert!(
        gone.is_empty(),
        "the catalog no longer names these providers, so their overlay entries can never apply: {gone:?}"
    );
    assert!(
        redundant.is_empty(),
        "the catalog caught up with these overlay entries; delete them:\n{}",
        redundant.join("\n")
    );
    println!(
        "catalog drift: {} overlay addition(s) still needed, {} field group(s) to trim",
        additions,
        review.len()
    );
}
