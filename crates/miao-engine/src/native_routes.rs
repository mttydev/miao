//! The native single-binary start derives its own host route manifest.
//!
//! The TypeScript host used to prepare `--model-routes-config` through its
//! engine shim: one route per configured provider model with a live stored
//! credential, mapped onto the engine's native transports. A native start has
//! no host, so the engine builds the same manifest from the operator's config
//! `providers` and the product credential store. Every provider the config
//! names but no credential covers is skipped — a route without a secret
//! cannot serve a turn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config_derive::{self, Source};

/// Build the manifest for these config sources and write it next to the
/// engine's storage. `Ok(None)` means nothing was routable: the config names
/// no providers, or none of them carries both a known transport and a live
/// credential. The caller keeps its fallback provider in that case.
pub fn materialize(
    sources: &[Source],
    credential_db: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let Some(entries) = routes(sources, credential_db)? else {
        return Ok(None);
    };
    let Some(root) = crate::placement::storage_root() else {
        return Ok(None);
    };
    let directory = root.join("native");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let path = directory.join("model-routes.json");
    // The storage root lives outside every workspace, which
    // `model_routes::read` requires of the manifest path.
    let body = serde_json::to_vec_pretty(&entries).map_err(|error| error.to_string())?;
    std::fs::write(&path, body).map_err(|error| error.to_string())?;
    Ok(Some(path))
}

/// Every route this configuration and credential store can address, in the
/// shape the manifest carries. `miao models` prints the same set, so the CLI
/// and a started engine cannot disagree about what a turn can reach.
pub fn routes(
    sources: &[Source],
    credential_db: Option<&Path>,
) -> Result<Option<Vec<Value>>, String> {
    routes_with(
        sources,
        credential_db,
        crate::model_catalog::models_snapshot().as_deref(),
    )
}

fn routes_with(
    sources: &[Source],
    credential_db: Option<&Path>,
    snapshot: Option<&Value>,
) -> Result<Option<Vec<Value>>, String> {
    let documents = config_derive::load(sources)?;
    let values = documents
        .iter()
        .map(|document| document.value.clone())
        .collect::<Vec<_>>();
    let merged = crate::config_scope::merge_values(&values);
    // A configuration that declares no provider still serves the providers its
    // stored keys cover: the snapshot carries their transport and models, so a
    // stored key is the operator saying "serve this one". Reading the section
    // as empty instead of absent is what lets both halves share one derivation.
    let empty = serde_json::Map::new();
    let providers = merged
        .get("providers")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let selected = merged.get("model").and_then(Value::as_str);
    // A missing or unreadable credential store leaves every provider without
    // a secret, which routes nothing — the same answer an empty store gives.
    let credentials = match credential_db.map(crate::credential::list_database) {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };
    let live = accounts_by_integration(&credentials);
    let mut entries = Vec::new();
    let mut primary_seen = false;
    for (id, provider) in providers {
        // A configuration entry and the catalog describe one provider: the
        // entry carries what the operator chose (a plan, an endpoint, extra
        // models) while the catalog carries the shipped model list. Merging
        // keeps a plan-only entry from replacing the catalog with an empty
        // model map.
        let provider = match snapshot
            .and_then(Value::as_object)
            .and_then(|snapshot| snapshot.get(id))
        {
            Some(base) => merge_provider(base, provider),
            None => provider.clone(),
        };
        let provider = &provider;
        if provider.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let accounts = live.get(id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        // A provider the store does not cover still serves turns when its key
        // arrives in the environment, which is how a shell-configured key has
        // always worked. The route is only written when the variable is
        // actually set: the engine refuses a whole manifest that names a
        // missing one, so an absent key has to route nothing rather than break
        // every other provider in the same file.
        let env_key = route_env_key(id);
        let from_environment = accounts.is_empty() && std::env::var_os(&env_key).is_some();
        if accounts.is_empty() && !from_environment {
            continue;
        }
        let provider_api = provider.get("api");
        let models = provider.get("models").and_then(Value::as_object);
        let Some(models) = models else { continue };
        for (model_id, model) in models {
            if model.get("disabled").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if !available(model) {
                continue;
            }
            // A model's own api overrides the provider's, the way the
            // product's catalog builds it.
            let api = model
                .get("api")
                .filter(|api| api.get("type").is_some() || api.get("url").is_some());
            let api = api.or(provider_api);
            let Some(api) = api else { continue };
            let Some((transport, endpoint)) = route_transport(id, api, model_id) else {
                continue;
            };
            let key = format!("{id}/{model_id}");
            let primary = selected == Some(key.as_str());
            primary_seen |= primary;
            let credential = match (from_environment, credential_db) {
                (true, _) => json!({ "type": "environment", "key": env_key }),
                (false, Some(db_path)) => json!({
                    "type": "database",
                    "path": db_path.display().to_string(),
                    // Every account answers through its own route, and
                    // `route_entry` replaces this id per account.
                    "id": accounts[0].id,
                    "integration": accounts[0].integration,
                }),
                (false, None) => continue,
            };
            let mut entry = json!({
                "id": key,
                "provider": transport,
                "model": model_id,
                "credential": credential,
                "primary": primary,
            });
            gateway_session_header(id, &mut entry);
            if let Some(endpoint) = endpoint {
                entry["endpoint"] = json!(endpoint);
            }
            if let Some(input) = model
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("input"))
                .and_then(Value::as_array)
                .filter(|input| !input.is_empty())
            {
                entry["input_modalities"] = json!(input);
            }
            if let Some(limit) = model.get("limit").and_then(Value::as_object) {
                let mut bounded = serde_json::Map::new();
                for field in ["context", "output"] {
                    if let Some(size) = limit.get(field).and_then(Value::as_u64) {
                        bounded.insert(field.into(), json!(size));
                    }
                }
                if !bounded.is_empty() {
                    entry["limit"] = Value::Object(bounded);
                }
            }
            if from_environment {
                if plan_allows(plan_for(provider, "default").as_deref(), model.get("plans")) {
                    entries.push(entry);
                }
                continue;
            }
            // A provider can hold keys on different plans, so the plan filter
            // runs per account. Only the provider's default account keeps the
            // plain route id a Session may already carry; a model its plan
            // filters out must not promote another account into that slot.
            let default_account = accounts.first().map(|account| account.id.as_str());
            for account in accounts {
                if !plan_allows(
                    plan_for(provider, &account.label).as_deref(),
                    model.get("plans"),
                ) {
                    continue;
                }
                let position = usize::from(Some(account.id.as_str()) != default_account);
                let mut base = entry.clone();
                // An OpenAI account grant belongs to the ChatGPT subscription
                // backend, not the API-key endpoint. Its transport supplies the
                // ChatGPT host and account header, so an explicit
                // api.openai.com endpoint would send the token to the wrong
                // service.
                if id == "openai" && account.kind == "oauth" {
                    base["provider"] = json!("subscription-responses");
                    if let Some(object) = base.as_object_mut() {
                        object.remove("endpoint");
                    }
                }
                entries.push(route_entry(
                    &base, id, model_id, account, position, &entries,
                ));
            }
        }
    }
    // Credential integrations the configuration does not name answer through
    // the catalog snapshot, the way the product host answers them.
    entries.extend(catalog_key_routes_with(snapshot, providers, credential_db));
    if entries.is_empty() {
        return Ok(None);
    }
    // The engine picks the primary entry as the default route; a config that
    // names no model falls back to the first provider the config declares.
    if !primary_seen {
        entries[0]["primary"] = json!(true);
    }
    Ok(Some(entries))
}

/// OpenCode's gateway routes a turn by conversation and refuses one that does
/// not name it (400 MissingSessionID), so every `opencode-go` route carries
/// the Session's opaque digest. The engine substitutes `$session` at send
/// time, keeping sibling Sessions distinct and matching the header the
/// product's own TypeScript and CLI routes send.
fn gateway_session_header(provider: &str, entry: &mut Value) {
    if provider == "opencode-go" {
        entry["auth"] = json!({ "headers": { "x-opencode-session": "$session" } });
    }
}

/// The environment variable a provider's key is normally supplied in. The
/// catalog names OpenCode's gateway `OPENCODE_API_KEY`, so the mechanical
/// `PROVIDER_API_KEY` spelling would look for `OPENCODE_GO_API_KEY` and find
/// nothing.
pub fn route_env_key(provider_id: &str) -> String {
    match provider_id {
        "opencode-go" => "OPENCODE_API_KEY".to_string(),
        _ => format!("{}_API_KEY", provider_id.to_uppercase().replace('-', "_")),
    }
}

/// Every unexpired credential per integration, the default label first: the
/// engine models an account as a route, so a provider holding two keys has to
/// answer through two routes. Keeping the first account on the plain
/// `provider/model` id leaves a Session that already pinned that id working.
fn accounts_by_integration(
    credentials: &[crate::credential::Metadata],
) -> BTreeMap<&str, Vec<&crate::credential::Metadata>> {
    let mut live: BTreeMap<&str, Vec<&crate::credential::Metadata>> = BTreeMap::new();
    for credential in credentials.iter().filter(|credential| !credential.expired) {
        live.entry(credential.integration.as_str())
            .or_default()
            .push(credential);
    }
    for accounts in live.values_mut() {
        accounts.sort_by(|left, right| {
            let rank =
                |credential: &crate::credential::Metadata| u8::from(credential.label != "default");
            rank(left)
                .cmp(&rank(right))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
    live
}

/// The catalogue half of the derivation: every provider the configuration does
/// not name, routed by the credentials the product store already holds.
///
/// A stored credential is the operator saying "serve this provider", so a
/// launcher has to turn it into a route even when no config entry mentions it.
/// The snapshot supplies the transport, endpoint and models, which is why
/// nothing had to be configured for this to work — and why leaving it out made
/// a stored key look unsaved until the operator edited their configuration by
/// hand. Account grants route through the transport they actually speak.
pub fn catalog_key_routes(
    declared: &serde_json::Map<String, Value>,
    credential_db: Option<&Path>,
) -> Vec<Value> {
    catalog_key_routes_with(
        crate::model_catalog::models_snapshot().as_deref(),
        declared,
        credential_db,
    )
}

fn catalog_key_routes_with(
    snapshot: Option<&Value>,
    declared: &serde_json::Map<String, Value>,
    credential_db: Option<&Path>,
) -> Vec<Value> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut entries = snapshot_key_routes(snapshot, declared, credential_db);
    entries.extend(snapshot_oauth_routes(snapshot, declared, credential_db));
    entries
}

/// Rewrite the manifest from the configuration and the credential store, and
/// answer how many entries changed.
///
/// The launcher writes the manifest once, before the engine starts, and an
/// engine restart re-reads exactly that file. Reconciling entry by entry cannot
/// keep it true: storing a key under an existing label replaces the row, so the
/// credential id changes, and a route still naming the old id is a route that
/// can only fail. Re-deriving the whole list from the same inputs the launch
/// used is the only answer that cannot drift — a stored key, a replaced key and
/// a second account all reach the engine the same way.
///
/// The primary route is carried over by id when it still exists, because the
/// operator's chosen model is not something a key edit may change.
pub fn refresh_routes(
    path: &Path,
    sources: &[Source],
    credential_db: Option<&Path>,
) -> Result<usize, String> {
    let previous = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let chosen = previous
        .iter()
        .find(|entry| entry.get("primary").and_then(Value::as_bool) == Some(true))
        .and_then(|entry| entry.get("id").and_then(Value::as_str))
        .map(str::to_string);
    let mut derived = routes(sources, credential_db)?.unwrap_or_default();
    // A route the operator had selected keeps the default when the new
    // derivation still serves it; otherwise the derivation's own choice stands.
    if let Some(chosen) = chosen {
        if derived
            .iter()
            .any(|entry| entry.get("id").and_then(Value::as_str) == Some(chosen.as_str()))
        {
            for entry in derived.iter_mut() {
                entry["primary"] = json!(entry.get("id").and_then(Value::as_str) == Some(&chosen));
            }
        }
    }
    let changed = previous.len() != derived.len()
        || previous
            .iter()
            .zip(derived.iter())
            .any(|(before, after)| before != after);
    if !changed {
        return Ok(0);
    }
    let body = serde_json::to_vec_pretty(&derived).map_err(|error| error.to_string())?;
    std::fs::write(path, body).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(derived.len().abs_diff(previous.len()).max(1))
}

fn snapshot_key_routes(
    snapshot: &Value,
    declared: &serde_json::Map<String, Value>,
    credential_db: Option<&Path>,
) -> Vec<Value> {
    let Some(db_path) = credential_db else {
        return Vec::new();
    };
    let Ok(credentials) = crate::credential::list_database(db_path) else {
        return Vec::new();
    };
    let live = accounts_by_integration(&credentials);
    let empty = serde_json::Map::new();
    let snapshot_providers = snapshot.as_object().unwrap_or(&empty);
    let mut entries: Vec<Value> = Vec::new();
    for (id, provider) in snapshot_providers {
        if declared.contains_key(id) {
            continue;
        }
        // An OAuth grant speaks its own transport, so only key credentials
        // route a snapshot provider here.
        let Some(accounts) = live
            .get(id.as_str())
            .map(|accounts| {
                accounts
                    .iter()
                    .filter(|account| account.kind == "key")
                    .copied()
                    .collect::<Vec<_>>()
            })
            .filter(|accounts| !accounts.is_empty())
        else {
            continue;
        };
        let Some(account) = accounts.first() else {
            continue;
        };
        let npm = provider.get("npm").and_then(Value::as_str);
        if npm != Some("@ai-sdk/openai-compatible") {
            continue;
        }
        let Some(base) = provider.get("api").and_then(Value::as_str) else {
            continue;
        };
        let models = provider.get("models").and_then(Value::as_object);
        let Some(models) = models else { continue };
        for (model_id, model) in models {
            // A catalog entry can omit `modalities` entirely (Command Code
            // publishes neither capabilities nor pricing on its model list).
            // An omitted field is unknown, not "no text": hiding an id the
            // account can call is worse than showing one it cannot.
            if !text_output(model) || !available(model) {
                continue;
            }
            let (transport, endpoint) = if id == "commandcode" {
                match commandcode_transport(base, model_id) {
                    Some(pair) => pair,
                    None => continue,
                }
            } else {
                match base.join_pair("chat/completions") {
                    Some(endpoint) => ("openai-chat".to_string(), Some(endpoint)),
                    None => continue,
                }
            };
            let mut entry = json!({
                "id": format!("{id}/{model_id}"),
                "provider": transport,
                "model": model_id,
                "credential": {
                    "type": "database",
                    "path": db_path.display().to_string(),
                    "id": account.id,
                    "integration": account.integration,
                },
            });
            if let Some(endpoint) = endpoint {
                entry["endpoint"] = json!(endpoint);
            }
            gateway_session_header(id, &mut entry);
            let limit = model.get("limit").and_then(Value::as_object);
            let mut bounded = serde_json::Map::new();
            for field in ["context", "output"] {
                if let Some(size) = limit
                    .and_then(|limit| limit.get(field))
                    .and_then(Value::as_u64)
                {
                    bounded.insert(field.into(), json!(size));
                }
            }
            if !bounded.is_empty() {
                entry["limit"] = Value::Object(bounded);
            }
            if let Some(input) = model
                .get("modalities")
                .and_then(|modalities| modalities.get("input"))
                .and_then(Value::as_array)
                .filter(|input| !input.is_empty())
            {
                entry["input_modalities"] = json!(input);
            }
            for (position, account) in accounts.iter().enumerate() {
                entries.push(route_entry(
                    &entry, id, model_id, account, position, &entries,
                ));
            }
        }
    }
    entries
}

/// Whether a model's declared output can carry text. A missing or empty
/// declaration is unknown, so the model stays routable; only an explicit
/// non-text output (an image model, for example) is skipped.
fn text_output(model: &Value) -> bool {
    match model
        .get("modalities")
        .and_then(|modalities| modalities.get("output"))
        .and_then(Value::as_array)
    {
        None => true,
        Some(outputs) => outputs.is_empty() || outputs.iter().any(|output| output == "text"),
    }
}

/// The catalog marks retired models `deprecated`; the product treats those as
/// disabled, so a derived route must not keep offering them.
fn available(model: &Value) -> bool {
    model.get("status").and_then(Value::as_str) != Some("deprecated")
}

/// The plan tiers Command Code uses to gate its catalog, cheapest first.
const PLAN_TIERS: [&str; 4] = ["go", "goat", "pro", "max"];

/// The plan an account is on: the account label's entry wins, then the
/// provider-wide plan. An absent value means "unknown", which hides nothing.
fn plan_for(provider: &Value, account: &str) -> Option<String> {
    provider
        .get("account_plans")
        .and_then(Value::as_object)
        .and_then(|plans| plans.get(account))
        .and_then(Value::as_str)
        .or_else(|| provider.get("plan").and_then(Value::as_str))
        .map(normalize_plan)
}

fn normalize_plan(plan: &str) -> String {
    match plan.trim().to_ascii_lowercase().as_str() {
        // Command Code sells Max as 10x and 20x; both name the catalog's `max`
        // tier. OpenCode's Go Plus and Go Teams likewise serve Go's models.
        "max10" | "max-10" | "max-10x" | "max_10x" | "max 10x" => "max".to_string(),
        "max20" | "max-20" | "max-20x" | "max_20x" | "max 20x" => "max".to_string(),
        "goplus" | "go-plus" | "go plus" | "goteams" | "go-teams" | "go teams" => "go".to_string(),
        other => other.to_string(),
    }
}

/// A model is selectable when its entry names no plan tiers, or names the
/// account's tier. An unrecognized plan (Teams, Provider, Enterprise) hides
/// nothing: a wrong model list is worse than a short one, but hiding a model
/// the account can call is worse still.
fn plan_allows(plan: Option<&str>, plans: Option<&Value>) -> bool {
    let Some(plan) = plan else {
        return true;
    };
    if !PLAN_TIERS.contains(&plan) {
        return true;
    }
    let Some(plans) = plans.and_then(Value::as_array) else {
        return true;
    };
    plans.is_empty()
        || plans
            .iter()
            .filter_map(Value::as_str)
            .any(|tier| normalize_plan(tier) == plan)
}

/// Merge a configured provider over its catalog entry. Only the top level is
/// replaced field by field; models are merged per id so a config that adds one
/// model or overrides one field keeps the catalog's other models.
fn merge_provider(base: &Value, declared: &Value) -> Value {
    let (Some(base_fields), Some(declared_fields)) = (base.as_object(), declared.as_object())
    else {
        return declared.clone();
    };
    let mut merged = base_fields.clone();
    let mut models = merged
        .remove("models")
        .and_then(|models| models.as_object().cloned())
        .unwrap_or_default();
    for (field, value) in declared_fields {
        if field == "models" {
            // An explicit model list is the operator's catalog for this
            // provider. Only an absent or empty list inherits the shipped
            // models, which is what a plan-only entry needs.
            let Some(declared_models) = value.as_object() else {
                continue;
            };
            if declared_models.is_empty() {
                continue;
            }
            let mut replaced = serde_json::Map::new();
            for (model_id, model) in declared_models {
                let merged_model = match models.get(model_id) {
                    Some(base_model) => merge_model(base_model, model),
                    None => model.clone(),
                };
                replaced.insert(model_id.clone(), merged_model);
            }
            models = replaced;
            continue;
        }
        merged.insert(field.clone(), value.clone());
    }
    merged.insert("models".into(), Value::Object(models));
    Value::Object(merged)
}

fn merge_model(base: &Value, declared: &Value) -> Value {
    let (Some(base_fields), Some(declared_fields)) = (base.as_object(), declared.as_object())
    else {
        return declared.clone();
    };
    let mut merged = base_fields.clone();
    for (field, value) in declared_fields {
        merged.insert(field.clone(), value.clone());
    }
    Value::Object(merged)
}

/// Pick the wire for one route. Command Code needs its own base and splits
/// Claude behind `/messages`; every other provider keeps the existing mapping.
fn route_transport(
    provider: &str,
    api: &Value,
    model_id: &str,
) -> Option<(String, Option<String>)> {
    let owned;
    let api = match api {
        Value::String(url) => {
            owned = json!({ "url": url });
            &owned
        }
        api => api,
    };
    if provider == "commandcode" {
        let base = api
            .as_str()
            .or_else(|| api.get("url").and_then(Value::as_str))?;
        return commandcode_transport(base, model_id);
    }
    transport(api, model_id)
}

/// Command Code's Provider API lives under `/provider/v1`; the catalog names
/// only the host. OpenAI-compatible models answer `/chat/completions`, while
/// the Claude models are served only by `/messages` (official model list
/// reports `supported_endpoints` per model).
fn commandcode_transport(base: &str, model_id: &str) -> Option<(String, Option<String>)> {
    let mut base = base.trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/chat/completions") {
        base = stripped;
    }
    if let Some(stripped) = base.strip_suffix("/messages") {
        base = stripped;
    }
    let base = match base.ends_with("/provider/v1") {
        true => base.to_string(),
        false => format!("{base}/provider/v1"),
    };
    if model_id.starts_with("claude") {
        base.join_pair("messages")
            .map(|endpoint| ("anthropic".to_string(), Some(endpoint)))
    } else {
        base.join_pair("chat/completions")
            .map(|endpoint| ("openai-chat".to_string(), Some(endpoint)))
    }
}
/// The account half of the catalogue fallback. An OpenAI account grant is not
/// an API key: it answers through the ChatGPT subscription transport, with the
/// models the shipped catalogue names for OpenAI. Without this, signing in
/// stored a credential no route could spend until the operator wrote an
/// `openai` provider entry by hand.
fn snapshot_oauth_routes(
    snapshot: &Value,
    declared: &serde_json::Map<String, Value>,
    credential_db: Option<&Path>,
) -> Vec<Value> {
    if declared.contains_key("openai") {
        return Vec::new();
    }
    let Some(db_path) = credential_db else {
        return Vec::new();
    };
    let Ok(credentials) = crate::credential::list_database(db_path) else {
        return Vec::new();
    };
    let accounts: Vec<&crate::credential::Metadata> = credentials
        .iter()
        .filter(|credential| {
            credential.integration == "openai" && credential.kind == "oauth" && !credential.expired
        })
        .collect();
    if accounts.is_empty() {
        return Vec::new();
    }
    let Some(models) = snapshot
        .get("openai")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut entries: Vec<Value> = Vec::new();
    for (model_id, model) in models {
        let produces_text = model
            .get("modalities")
            .and_then(|modalities| modalities.get("output"))
            .and_then(Value::as_array)
            .is_some_and(|outputs| outputs.iter().any(|item| item == "text"));
        if !produces_text {
            continue;
        }
        let base = json!({
            "id": format!("openai/{model_id}"),
            "provider": "subscription-responses",
            "model": model_id,
            "credential": {
                "type": "database",
                "path": db_path.display().to_string(),
                "id": accounts[0].id,
                "integration": "openai",
            },
        });
        for (position, account) in accounts.iter().enumerate() {
            entries.push(route_entry(
                &base, "openai", model_id, account, position, &entries,
            ));
        }
    }
    entries
}

/// The wire a configured api rides, and the endpoint it reaches. The mapping
/// mirrors the product's route planner: an explicit URL wins over the
/// package's default, the suffix is appended once, and URLs carrying
/// credentials in the query or userinfo are refused.
fn transport(api: &Value, model_id: &str) -> Option<(String, Option<String>)> {
    let url = api.get("url").and_then(Value::as_str);
    match api.get("type").and_then(Value::as_str) {
        // Plugin catalogs without an AI SDK package declare an
        // OpenAI-compatible base URL directly.
        Some("native") => {
            let base = url?.trim_end_matches('/');
            let suffix = "chat/completions";
            let endpoint = if base.ends_with(suffix) {
                base.to_string()
            } else {
                format!("{base}/{suffix}")
            };
            Some(("openai-chat".into(), Some(endpoint)))
        }
        Some("aisdk") => {
            let package = api.get("package").and_then(Value::as_str)?;
            let defaults: &[(&str, &str)] = &[
                ("@ai-sdk/deepseek", "https://api.deepseek.com/v1"),
                ("@ai-sdk/groq", "https://api.groq.com/openai/v1"),
                ("@ai-sdk/mistral", "https://api.mistral.ai/v1"),
                ("@ai-sdk/xai", "https://api.x.ai/v1"),
            ];
            let base = url
                .map(|url| url.trim_end_matches('/').to_string())
                .or_else(|| {
                    defaults
                        .iter()
                        .find(|(name, _)| *name == package)
                        .map(|(_, base)| base.to_string())
                });
            match package {
                "@ai-sdk/anthropic" => {
                    let endpoint = base.as_deref().unwrap_or("https://api.anthropic.com/v1");
                    endpoint
                        .join_pair("messages")
                        .map(|url| ("anthropic".into(), Some(url)))
                }
                "@ai-sdk/google" => {
                    let endpoint = base
                        .as_deref()
                        .unwrap_or("https://generativelanguage.googleapis.com/v1beta");
                    let suffix = format!(
                        "models/{}:streamGenerateContent",
                        model_id.trim_start_matches("models/")
                    );
                    endpoint
                        .join_pair(&suffix)
                        .map(|url| ("gemini".into(), Some(url)))
                }
                "@ai-sdk/openai" => {
                    let endpoint = base.as_deref().unwrap_or("https://api.openai.com/v1");
                    endpoint
                        .join_pair("responses")
                        .map(|url| ("openai-responses".into(), Some(url)))
                }
                "@ai-sdk/amazon-bedrock" => Some(("amazon-bedrock".into(), url.map(str::to_owned))),
                // The OpenAI-compatible family, including every package whose
                // default URL is itself chat-completions shaped.
                "@ai-sdk/openai-compatible"
                | "@ai-sdk/deepseek"
                | "@ai-sdk/groq"
                | "@ai-sdk/mistral"
                | "@ai-sdk/xai" => {
                    let endpoint = base?;
                    endpoint
                        .join_pair("chat/completions")
                        .map(|url| ("openai-chat".into(), Some(url)))
                }
                _ => None,
            }
        }
        // A hand-written entry that names only a base URL, or a wire older than
        // the `aisdk`/`native` split, is an OpenAI-compatible endpoint. The
        // product planner treated it that way, and configurations written then
        // still carry it; refusing them would drop providers that serve turns
        // today.
        _ => url
            .and_then(|base| base.trim_end_matches('/').join_pair("chat/completions"))
            .map(|endpoint| ("openai-chat".into(), Some(endpoint))),
    }
}

/// Append one path suffix to an http(s) base URL, refusing anything that
/// would carry a secret in the address the way the product planner does.
trait EndpointJoin {
    fn join_pair(self, suffix: &str) -> Option<String>;
}

impl EndpointJoin for &str {
    fn join_pair(self, suffix: &str) -> Option<String> {
        let mut url = reqwest::Url::parse(self).ok()?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return None;
        }
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let forbidden = |key: &str| {
            let key = key.to_ascii_lowercase();
            ["key", "token", "secret", "auth"]
                .iter()
                .any(|word| key.contains(word))
        };
        if url
            .query_pairs()
            .any(|(key, _)| forbidden(key.to_string().trim()))
        {
            return None;
        }
        let path = url.path().trim_end_matches('/').to_string();
        let next = if path.ends_with(&format!("/{suffix}")) {
            path
        } else {
            format!("{path}/{suffix}")
        };
        url.set_path(&next);
        Some(url.to_string())
    }
}

/// One route per account. The engine models an account as a route, so a
/// provider holding several keys answers through several routes; the first
/// keeps the plain `provider/model` id a Session may already carry, and the
/// rest name their account so they stay distinct and findable.
fn route_entry(
    base: &Value,
    provider: &str,
    model: &str,
    account: &crate::credential::Metadata,
    position: usize,
    entries: &[Value],
) -> Value {
    let key = format!("{provider}/{model}");
    let mut entry = base.clone();
    if position > 0 {
        let candidate = format!("{key}@{}", account_slot(&account.label));
        entry["id"] = json!(free_route_id(&candidate, entries));
        // Only the account that answers the plain id can be the primary route.
        entry["primary"] = json!(false);
    }
    entry["credential"]["id"] = json!(account.id);
    entry["credential"]["integration"] = json!(account.integration);
    entry["account"] = json!({
        "id": account.id,
        "model_key": key,
        "label": account.label,
        "default": position == 0,
    });
    entry
}

/// A label that can sit in a route id: ids travel through Session pins and
/// command arguments, so keep them to characters that need no quoting.
fn account_slot(label: &str) -> String {
    let slot: String = label
        .chars()
        .map(|character| {
            match character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                true => character,
                false => '-',
            }
        })
        .collect();
    let slot = slot.trim_matches('-').to_string();
    match slot.is_empty() {
        true => "account".to_string(),
        false => slot,
    }
}

/// Two labels can normalize to the same slot (`work!` and `work?`), so a route
/// id is only reused when nothing already claims it.
fn free_route_id(candidate: &str, entries: &[Value]) -> String {
    let taken = |id: &str| {
        entries
            .iter()
            .any(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
    };
    if !taken(candidate) {
        return candidate.to_string();
    }
    (2..)
        .map(|suffix| format!("{candidate}-{suffix}"))
        .find(|id| !taken(id))
        .expect("a free route id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_configured_provider_with_a_live_credential_becomes_a_route() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "model": "volcengine/deepseek",
                "providers": {
                    "volcengine": {
                        "models": {
                            "deepseek": {
                                "api": {
                                    "type": "aisdk",
                                    "package": "@ai-sdk/openai-compatible",
                                    "url": "https://ark.example.com/api/v3",
                                },
                                "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                                "limit": {"context": 1048576, "output": 393216},
                            },
                            "unknown-wire": {
                                "api": {"type": "aisdk", "package": "@ai-sdk/something-else"},
                            },
                        }
                    },
                    "uncovered": {
                        "models": {
                            "m": {"api": {"type": "aisdk", "package": "@ai-sdk/openai-compatible", "url": "https://x.example/v1"}},
                        }
                    },
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred1', 'volcengine', 'volc', '{\"kind\":\"key\"}')",
                [],
            )
            .unwrap();
        let entries = routes(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
        )
        .unwrap()
        .unwrap();
        // Only the covered provider's routable model survives; the unknown
        // wire and the credential-less provider contribute nothing.
        assert_eq!(
            entries.len(),
            1,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        let entry = &entries[0];
        assert_eq!(entry["id"], "volcengine/deepseek");
        assert_eq!(entry["provider"], "openai-chat");
        assert_eq!(
            entry["endpoint"],
            "https://ark.example.com/api/v3/chat/completions"
        );
        assert_eq!(entry["primary"], true);
        assert_eq!(entry["credential"]["integration"], "volcengine");
        assert_eq!(entry["input_modalities"], json!(["text"]));
        assert_eq!(entry["limit"]["context"], 1048576);
    }

    /// An OpenAI account grant must ride the ChatGPT subscription transport.
    /// Sending it to the API-key endpoint answers 401 however valid the token
    /// is, so the route itself has to change, not just the credential.
    #[test]
    fn an_openai_account_grant_routes_the_chatgpt_subscription_transport() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "model": "openai/gpt-5",
                "providers": {
                    "openai": {
                        "models": {
                            "gpt-5": {
                                "api": {"type": "aisdk", "package": "@ai-sdk/openai"},
                            }
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = directory.path().join("credentials.db");
        let grant = json!({
            "type": "oauth",
            "access": "a1",
            "refresh": "r1",
            "expires": 4102444800000u64,
            "methodID": "chatgpt-headless",
        });
        crate::credential::store_oauth(&database, "openai", "default", &grant).expect("store");
        let entries = routes(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        assert_eq!(entries[0]["provider"], "subscription-responses");
        assert_eq!(entries[0].get("endpoint"), None);

        // An API key beside the account keeps the ordinary Responses route.
        crate::credential::store_key(&database, "openai", "sk-live", Some("work")).expect("store");
        let entries = routes(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
        )
        .unwrap()
        .unwrap();
        let key = entries
            .iter()
            .find(|entry| entry["provider"] == "openai-responses")
            .expect("the key route");
        assert_eq!(key["endpoint"], "https://api.openai.com/v1/responses");
        assert!(entries
            .iter()
            .any(|entry| entry["provider"] == "subscription-responses"));
    }

    /// Signing in has to make OpenAI's models selectable even when the
    /// configuration never names the provider: the shipped catalogue supplies
    /// the models and the account grant supplies the transport.
    #[test]
    fn a_stored_account_grant_routes_openai_without_a_provider_entry() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("credentials.db");
        let grant = json!({
            "type": "oauth",
            "access": "a1",
            "refresh": "r1",
            "expires": 4102444800000u64,
            "methodID": "chatgpt-headless",
        });
        crate::credential::store_oauth(&database, "openai", "default", &grant).expect("store");
        let snapshot = json!({
            "openai": {
                "npm": "@ai-sdk/openai",
                "models": {
                    "gpt-5": {"modalities": {"output": ["text"]}},
                    "gpt-image": {"modalities": {"output": ["image"]}},
                },
            },
        });
        let entries = snapshot_oauth_routes(&snapshot, &serde_json::Map::new(), Some(&database));
        assert_eq!(
            entries.len(),
            1,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        assert_eq!(entries[0]["id"], "openai/gpt-5");
        assert_eq!(entries[0]["provider"], "subscription-responses");
        assert_eq!(entries[0].get("endpoint"), None);
        assert_eq!(entries[0]["credential"]["integration"], "openai");
    }

    /// A provider can hold several accounts, and the engine models an account as
    /// a route: both keys have to answer for the same model, with the default
    /// one on the plain id a Session may already have pinned.
    #[test]
    fn a_second_credential_adds_a_second_route_for_the_same_model() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "model": "volcengine/deepseek",
                "providers": {
                    "volcengine": {
                        "models": {
                            "deepseek": {
                                "api": {
                                    "type": "aisdk",
                                    "package": "@ai-sdk/openai-compatible",
                                    "url": "https://ark.example.com/api/v3",
                                },
                            }
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        // "default" sorts after "work" by id, so the label rank has to decide.
        for (id, label) in [("cred_b", "default"), ("cred_a", "work")] {
            connection
                .execute(
                    "INSERT INTO credential VALUES (?1, 'volcengine', ?2, '{\"type\":\"key\",\"key\":\"k\"}')",
                    rusqlite::params![id, label],
                )
                .unwrap();
        }
        let entries = routes(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            entries.len(),
            2,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        let default = entries
            .iter()
            .find(|entry| entry["id"] == "volcengine/deepseek")
            .expect("the default account keeps the plain id");
        assert_eq!(default["primary"], true);
        assert_eq!(default["credential"]["id"], "cred_b");
        assert_eq!(default["account"]["label"], "default");
        assert_eq!(default["account"]["default"], true);
        assert_eq!(default["account"]["model_key"], "volcengine/deepseek");
        let named = entries
            .iter()
            .find(|entry| entry["id"] == "volcengine/deepseek@work")
            .expect("the named account gets its own route");
        assert_eq!(named["primary"], false);
        assert_eq!(named["credential"]["id"], "cred_a");
        assert_eq!(named["account"]["label"], "work");
        assert_eq!(named["account"]["default"], false);
        assert_eq!(named["model"], "deepseek");
    }

    /// A key stored for a provider the configuration never names is the
    /// operator asking for that provider's models. It used to route nothing
    /// outside the native single-binary start, so the provider stayed
    /// unconfigured however often the key was re-entered.
    #[test]
    fn a_stored_key_routes_a_provider_the_config_never_names() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred_zhipu', 'zhipuai-coding-plan', 'default', '{\"type\":\"key\",\"key\":\"sk-test\"}')",
                [],
            )
            .unwrap();
        let snapshot = json!({
            "zhipuai-coding-plan": {
                "name": "Zhipu AI Coding Plan",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://open.bigmodel.cn/api/coding/paas/v4",
                "models": {
                    "glm-5.3": {
                        "tool_call": true,
                        "modalities": { "input": ["text"], "output": ["text"] },
                        "limit": { "context": 1000000, "output": 131072 },
                    },
                    "glm-image": {
                        "modalities": { "input": ["text"], "output": ["image"] },
                    },
                },
            },
        });
        let entries = snapshot_key_routes(&snapshot, &serde_json::Map::new(), Some(&database));
        // Only the model that answers in text becomes a route.
        assert_eq!(
            entries.len(),
            1,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        let entry = &entries[0];
        assert_eq!(entry["id"], "zhipuai-coding-plan/glm-5.3");
        assert_eq!(entry["provider"], "openai-chat");
        assert_eq!(
            entry["endpoint"],
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(entry["credential"]["id"], "cred_zhipu");
        assert_eq!(entry["credential"]["integration"], "zhipuai-coding-plan");
        // The primary is the launcher's decision: a catalog route never claims
        // it, so a configured default stays the default.
        assert_eq!(entry.get("primary"), None);
    }

    /// The configuration owns the providers it names. A stored key for the
    /// same integration must not add a second route under an id the config
    /// already routes.
    #[test]
    fn a_declared_provider_is_left_to_the_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred_volcengine', 'volcengine', 'default', '{\"type\":\"key\",\"key\":\"sk-test\"}')",
                [],
            )
            .unwrap();
        let snapshot = json!({
            "volcengine": {
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://ark.example.com/api/v3",
                "models": { "deepseek": { "modalities": { "output": ["text"] } } },
            },
        });
        let mut declared = serde_json::Map::new();
        declared.insert("volcengine".into(), json!({ "models": {} }));
        assert!(snapshot_key_routes(&snapshot, &declared, Some(&database)).is_empty());
    }

    /// A key stored while the TUI runs has to reach the manifest the engine
    /// re-reads on restart, and a replaced key must not detach the route it
    /// already served: `store_key` keeps the credential id, so re-deriving
    /// leaves the manifest unchanged instead of pointing at a dead id.
    #[test]
    fn a_refreshed_manifest_follows_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("credentials.db");
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "model": "vendor/deepseek",
                "providers": {
                    "vendor": {
                        "models": {
                            "deepseek": {
                                "api": {
                                    "type": "aisdk",
                                    "package": "@ai-sdk/openai-compatible",
                                    "url": "https://vendor.test/v1",
                                },
                            },
                        },
                    },
                },
            }))
            .unwrap(),
        )
        .unwrap();
        let sources = vec![Source::Dir(directory.path().to_path_buf())];
        crate::credential::store_key(&database, "vendor", "sk-first", None).expect("stores");
        let path = directory.path().join("routes.json");
        std::fs::write(&path, b"[]").unwrap();
        assert_eq!(
            refresh_routes(&path, &sources, Some(&database)).expect("refreshes"),
            1,
            "the stored key reaches the manifest"
        );
        let first: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(first[0]["id"], "vendor/deepseek");
        assert_eq!(first[0]["primary"], true);
        let id = first[0]["credential"]["id"].as_str().unwrap().to_string();
        // Replacing the key keeps its row, so the route keeps resolving and
        // nothing has to be rewritten.
        let replaced =
            crate::credential::store_key(&database, "vendor", "sk-second", None).expect("replaces");
        assert_eq!(replaced.id, id, "a replaced key keeps its credential id");
        assert_eq!(
            refresh_routes(&path, &sources, Some(&database)).expect("idempotent"),
            0
        );
        // A second account is a second route, named after its label.
        crate::credential::store_key(&database, "vendor", "sk-other", Some("work")).expect("adds");
        refresh_routes(&path, &sources, Some(&database)).expect("adds the account");
        let manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let ids: Vec<&str> = manifest
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["vendor/deepseek", "vendor/deepseek@work"]);
        assert_eq!(
            manifest[1]["account"]["label"], "work",
            "the account list is built from this metadata"
        );
        assert_eq!(
            manifest[0]["primary"], true,
            "the chosen route stays default"
        );
        // Deleting every key takes their routes with them.
        crate::credential::remove_key(&database, "vendor", None).expect("removes");
        refresh_routes(&path, &sources, Some(&database)).expect("prunes");
        let manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(manifest.as_array().expect("a list").len(), 0);
    }

    #[test]
    fn the_url_suffix_appends_once() {
        let url = "https://ark.example.com/api/v3"
            .join_pair("chat/completions")
            .unwrap();
        assert_eq!(url, "https://ark.example.com/api/v3/chat/completions");
        let already = "https://ark.example.com/api/v3/chat/completions"
            .join_pair("chat/completions")
            .unwrap();
        assert_eq!(already, "https://ark.example.com/api/v3/chat/completions");
    }

    #[test]
    fn a_url_carrying_secrets_in_its_query_is_refused() {
        assert!("https://host.example/v1?api-key=xyz"
            .join_pair("chat/completions")
            .is_none());
        assert!("https://user:pw@host.example/v1"
            .join_pair("chat/completions")
            .is_none());
        assert!("ftp://host.example/v1"
            .join_pair("chat/completions")
            .is_none());
    }

    /// OpenCode's gateway refuses a turn that does not name its conversation
    /// (400 MissingSessionID), so a derived `opencode-go` route has to carry
    /// the Session-bound header, and only that provider's routes do.
    #[test]
    fn an_opencode_go_route_carries_the_session_header() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "model": "opencode-go/deepseek",
                "providers": {
                    "opencode-go": {
                        "models": {
                            "deepseek": {
                                "api": {
                                    "type": "aisdk",
                                    "package": "@ai-sdk/openai-compatible",
                                    "url": "https://opencode.ai/inference/go/openai/v1",
                                },
                            }
                        }
                    },
                    "vendor": {
                        "models": {
                            "other": {
                                "api": {
                                    "type": "aisdk",
                                    "package": "@ai-sdk/openai-compatible",
                                    "url": "https://vendor.test/v1",
                                },
                            }
                        }
                    },
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred_go', 'opencode-go', 'default', '{\"kind\":\"key\"}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred_vendor', 'vendor', 'default', '{\"kind\":\"key\"}')",
                [],
            )
            .unwrap();
        let entries = routes(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
        )
        .unwrap()
        .unwrap();
        let gateway = entries
            .iter()
            .find(|entry| entry["id"] == "opencode-go/deepseek")
            .expect("a gateway route");
        assert_eq!(
            gateway["auth"]["headers"]["x-opencode-session"],
            "$session",
            "{}",
            serde_json::to_string(&gateway).unwrap()
        );
        let vendor = entries
            .iter()
            .find(|entry| entry["id"] == "vendor/other")
            .expect("a configured route");
        assert!(
            vendor.get("auth").is_none(),
            "{}",
            serde_json::to_string(&vendor).unwrap()
        );
    }

    /// The catalog half of the derivation routes a provider the configuration
    /// never names, and the gateway among them needs the same Session header.
    #[test]
    fn a_snapshot_gateway_route_names_the_session_too() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO credential VALUES ('cred_go', 'opencode-go', 'default', '{\"type\":\"key\",\"key\":\"sk-test\"}')",
                [],
            )
            .unwrap();
        let snapshot = json!({
            "opencode-go": {
                "name": "OpenCode Go",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://opencode.ai/inference/go/openai/v1",
                "models": {
                    "deepseek": {
                        "modalities": { "input": ["text"], "output": ["text"] },
                    },
                },
            },
        });
        let entries = snapshot_key_routes(&snapshot, &serde_json::Map::new(), Some(&database));
        assert_eq!(
            entries.len(),
            1,
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        assert_eq!(
            entries[0]["auth"]["headers"]["x-opencode-session"],
            "$session",
            "{}",
            serde_json::to_string(&entries[0]).unwrap()
        );
    }

    fn snapshot_database(directory: &Path, accounts: &[(&str, &str)]) -> std::path::PathBuf {
        let database = directory.join("credentials.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE credential (id text primary key, integration_id text, label text, value text)",
                [],
            )
            .unwrap();
        for (id, label) in accounts {
            connection
                .execute(
                    "INSERT INTO credential VALUES (?1, 'commandcode', ?2, '{\"type\":\"key\",\"key\":\"k\"}')",
                    rusqlite::params![id, label],
                )
                .unwrap();
        }
        database
    }

    /// Command Code's model list carries no capabilities, so only one model
    /// had `modalities` and the rest vanished from the picker. A missing
    /// declaration has to count as text, `deprecated` entries stay out, and
    /// Claude belongs on `/messages` while the Provider API lives under
    /// `/provider/v1`.
    #[test]
    fn commandcode_catalog_models_route_without_modalities() {
        let directory = tempfile::tempdir().unwrap();
        let database = snapshot_database(directory.path(), &[("cred_cc", "default")]);
        let snapshot = json!({
            "commandcode": {
                "name": "Command Code",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://api.commandcode.ai",
                "models": {
                    "inclusionai/ling-3.1-flash:free": {},
                    "deepseek/deepseek-v4.1-flash": {
                        "limit": { "context": 1000000, "output": 32768 },
                    },
                    "claude-opus-5": {},
                    "retired-model": { "status": "deprecated" },
                    "image-model": {
                        "modalities": { "input": ["text"], "output": ["image"] },
                    },
                },
            },
        });
        let entries = snapshot_key_routes(&snapshot, &serde_json::Map::new(), Some(&database));
        let ids: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "commandcode/claude-opus-5",
                "commandcode/deepseek/deepseek-v4.1-flash",
                "commandcode/inclusionai/ling-3.1-flash:free",
            ],
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
        let chat = &entries[1];
        assert_eq!(chat["provider"], "openai-chat");
        assert_eq!(
            chat["endpoint"],
            "https://api.commandcode.ai/provider/v1/chat/completions"
        );
        let claude = &entries[0];
        assert_eq!(claude["provider"], "anthropic");
        assert_eq!(
            claude["endpoint"],
            "https://api.commandcode.ai/provider/v1/messages"
        );
    }

    /// A plan-only provider entry must keep the shipped catalog, and two keys
    /// on different plans must see exactly the models their plan includes.
    #[test]
    fn commandcode_plans_filter_models_per_account() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("miao.json"),
            serde_json::to_string(&json!({
                "providers": {
                    "commandcode": {
                        "plan": "pro",
                        "account_plans": { "work": "max" },
                        "models": {},
                    },
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = snapshot_database(
            directory.path(),
            &[("cred_default", "default"), ("cred_work", "work")],
        );
        let snapshot = json!({
            "commandcode": {
                "name": "Command Code",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://api.commandcode.ai",
                "models": {
                    "all-plans": {},
                    "go-only": { "plans": ["go"] },
                    "pro-and-up": { "plans": ["pro", "max"] },
                    "max-only": { "plans": ["max"] },
                },
            },
        });
        let entries = routes_with(
            &[Source::Dir(directory.path().to_path_buf())],
            Some(&database),
            Some(&snapshot),
        )
        .unwrap()
        .unwrap();
        let ids: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "commandcode/all-plans",
                "commandcode/all-plans@work",
                "commandcode/max-only@work",
                "commandcode/pro-and-up",
                "commandcode/pro-and-up@work",
            ],
            "{}",
            serde_json::to_string(&entries).unwrap()
        );
    }

    #[test]
    fn plan_aliases_and_unknown_tiers_do_not_hide_models() {
        assert!(plan_allows(Some("max"), Some(&json!(["max"]))));
        assert!(plan_allows(Some("max-20x"), Some(&json!(["max"]))));
        assert!(plan_allows(Some("go plus"), Some(&json!(["go"]))));
        assert!(!plan_allows(Some("pro"), Some(&json!(["max"]))));
        assert!(plan_allows(Some("pro"), None));
        assert!(plan_allows(Some("team"), Some(&json!(["max"]))));
        assert!(plan_allows(None, Some(&json!(["max"]))));
    }
}
