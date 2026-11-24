//! Account-scoped model discovery.
//!
//! The served catalog lists what a provider publishes in public. A subscription
//! plan can serve more: the coding-plan gateway answers `/models` with the ids
//! the connected account can call, and a model can be live there before the
//! catalog lists it. This asks each provider this machine already holds a key
//! for, and merges the ids the catalog does not carry, so a new model becomes
//! selectable without waiting for a catalog edit or a release.
//!
//! Discovery never invents a provider, never overrides a catalog entry, and
//! never runs on a start: the product refreshes it in the background and the
//! snapshot merge reads the cache.

use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

/// How long one discovery answer is reused.
pub const TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Most ids one provider may contribute: a plan endpoint answers with what the
/// account can call, and the route catalog is bounded.
const MAX_MODELS: usize = 256;
/// Discovery answers are id lists; anything this large is not one.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// What one refresh found, for `miao models --refresh` to report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub providers: usize,
    pub models: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "account discovery: {} model(s) across {} provider(s)",
            self.models, self.providers
        )
    }
}

/// Where discovered ids are cached, beside the fetched catalog.
pub fn cache_file() -> Option<PathBuf> {
    Some(crate::model_catalog::cache_dir()?.join("discovered.json"))
}

fn read_cache(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Merge the cached discovery into a catalog snapshot: only for providers the
/// catalog already names, and only ids it does not already carry. Everything
/// discovery adds is a text-in/text-out entry with no declared window or price;
/// the overlay runs after it, so a maintained entry outranks a guess.
pub fn merge(snapshot: &mut Value) {
    let Some(path) = cache_file() else {
        return;
    };
    merge_from(snapshot, &read_cache(&path));
}

fn merge_from(snapshot: &mut Value, cached: &serde_json::Map<String, Value>) {
    let Some(providers) = snapshot.as_object_mut() else {
        return;
    };
    for (provider_id, entry) in cached {
        let Some(models) = providers
            .get_mut(provider_id.as_str())
            .and_then(|provider| provider.get_mut("models"))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let Some(ids) = entry.get("models").and_then(Value::as_array) else {
            continue;
        };
        for id in ids.iter().filter_map(Value::as_str).take(MAX_MODELS) {
            if !valid_model_id(id) || models.contains_key(id) {
                continue;
            }
            models.insert(
                id.to_string(),
                json!({
                    "name": id,
                    "discovered": true,
                    "modalities": { "input": ["text"], "output": ["text"] },
                }),
            );
        }
    }
}

/// Ask every provider this machine holds an unexpired key for what models its
/// account serves, and cache the answers.
///
/// Only OpenAI-compatible providers are probed: their `/models` answer is the
/// `data[].id` shape every other transport does not share, and a wrong shape
/// would teach the catalog nothing.
///
/// An answer younger than [`TTL`] is reused, so a background refresh costs one
/// request per provider per window; `force` probes everything.
pub async fn refresh(db: &Path, force: bool) -> Result<Summary, String> {
    let snapshot = crate::model_catalog::models_snapshot().ok_or("no catalog snapshot")?;
    let Some(path) = cache_file() else {
        return Err("no cache directory".into());
    };
    let credentials = crate::credential::list_database(db).map_err(|error| error.to_string())?;
    let client = reqwest::Client::builder()
        .user_agent(format!(
            "miao/{}",
            option_env!("MIAO_CLI_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
        ))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let mut cached = read_cache(&path);
    let mut summary = Summary::default();
    let providers = snapshot.as_object().cloned().unwrap_or_default();
    for (provider_id, provider) in &providers {
        if !force {
            let fresh = cached
                .get(provider_id)
                .and_then(|entry| entry.get("fetched_at_ms"))
                .and_then(Value::as_u64)
                .is_some_and(|fetched| now_ms().saturating_sub(fetched) < TTL.as_millis() as u64);
            if fresh {
                continue;
            }
        }
        let Some(api) = provider.get("api").and_then(Value::as_str) else {
            continue;
        };
        let Some(transport) = provider
            .get("npm")
            .and_then(Value::as_str)
            .and_then(crate::provider::transport_for_package)
        else {
            continue;
        };
        if transport != "openai-chat" {
            continue;
        }
        let Some(credential) = credentials
            .iter()
            .find(|credential| credential.integration == *provider_id && !credential.expired)
        else {
            continue;
        };
        let source = crate::credential::Source::Database {
            path: db.to_path_buf(),
            id: credential.id.clone(),
            integration: provider_id.clone(),
        };
        let Ok(secret) = source.load().await else {
            continue;
        };
        let Ok(ids) = fetch_ids(&client, api, secret.secret()).await else {
            continue;
        };
        summary.providers += 1;
        summary.models += ids.len();
        cached.insert(
            provider_id.clone(),
            json!({ "fetched_at_ms": now_ms(), "models": ids }),
        );
    }
    if summary.providers > 0 {
        let bytes =
            serde_json::to_vec(&Value::Object(cached)).map_err(|error| error.to_string())?;
        crate::model_catalog::write_atomic(&path, &bytes).map_err(|error| error.to_string())?;
    }
    Ok(summary)
}

/// One provider's model list.
pub(crate) async fn fetch_ids(
    client: &reqwest::Client,
    api: &str,
    secret: &str,
) -> Result<Vec<String>, String> {
    let response = client
        .get(models_url(api))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {secret}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    if bytes.len() > MAX_BODY {
        return Err("the model list answered with more than 4 MiB".into());
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(ids_from_body(&body))
}

/// The `/models` URL for a provider API base.
pub fn models_url(api: &str) -> String {
    let api = api.trim_end_matches('/');
    match api.ends_with("/models") {
        true => api.to_string(),
        false => format!("{api}/models"),
    }
}

/// Ids a model-list answer carries: OpenAI's `data[].id` and the bare string
/// list some gateways answer with.
pub fn ids_from_body(body: &Value) -> Vec<String> {
    let list = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array);
    let mut ids = BTreeSet::new();
    for item in list.into_iter().flatten().take(MAX_MODELS) {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.as_str());
        if let Some(id) = id.map(str::trim).filter(|id| valid_model_id(id)) {
            ids.insert(id.to_string());
        }
    }
    ids.into_iter().collect()
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{ids_from_body, merge_from, models_url};
    use serde_json::json;
    use std::io::{Read, Write};

    #[test]
    fn an_openai_model_list_yields_its_ids() {
        let body = json!({
            "object": "list",
            "data": [
                { "id": "glm-5.4", "object": "model" },
                { "id": "glm-5.3" },
                { "id": "" },
                { "id": "glm-5.4" },
                "glm-4.6",
            ],
        });
        assert_eq!(
            ids_from_body(&body),
            vec!["glm-4.6".to_string(), "glm-5.3".into(), "glm-5.4".into()]
        );
    }

    #[test]
    fn a_body_without_a_model_list_yields_nothing() {
        assert!(ids_from_body(&json!({ "error": "rate limited" })).is_empty());
        assert!(ids_from_body(&json!({ "data": "nope" })).is_empty());
    }

    #[test]
    fn the_models_url_appends_the_path_once() {
        assert_eq!(
            models_url("https://api.test/v1"),
            "https://api.test/v1/models"
        );
        assert_eq!(
            models_url("https://api.test/v1/"),
            "https://api.test/v1/models"
        );
        assert_eq!(
            models_url("https://api.test/v1/models"),
            "https://api.test/v1/models"
        );
    }

    #[test]
    fn merging_adds_only_what_the_catalog_misses() {
        let mut snapshot = json!({
            "plan": { "models": {
                "glm-5.3": { "name": "GLM-5.3", "limit": { "context": 200000 } },
            } },
        });
        let cached = json!({
            "plan": { "fetched_at_ms": 1, "models": ["glm-5.3", "glm-5.4"] },
            "unlisted": { "fetched_at_ms": 1, "models": ["gone"] },
        });
        merge_from(&mut snapshot, cached.as_object().expect("object"));
        assert_eq!(
            snapshot["plan"]["models"]["glm-5.3"]["limit"]["context"],
            json!(200000),
            "a catalog entry is never replaced by a discovered one"
        );
        assert_eq!(
            snapshot["plan"]["models"]["glm-5.4"]["discovered"],
            json!(true)
        );
        assert_eq!(
            snapshot["plan"]["models"]["glm-5.4"]["modalities"]["input"],
            json!(["text"])
        );
        assert!(
            snapshot.get("unlisted").is_none(),
            "discovery never invents a provider"
        );
    }

    #[tokio::test]
    async fn a_probe_sends_the_key_and_reads_the_ids() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_lowercase();
            let body = r#"{"data":[{"id":"glm-5.4"}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("write");
            request
        });
        let client = reqwest::Client::new();
        let ids = super::fetch_ids(&client, &format!("http://{address}/v1"), "secret-key")
            .await
            .expect("ids");
        assert_eq!(ids, vec!["glm-5.4".to_string()]);
        let request = server.join().expect("server");
        assert!(
            request.contains("authorization: bearer secret-key"),
            "{request}"
        );
        assert!(request.contains("get /v1/models"), "{request}");
    }
}
