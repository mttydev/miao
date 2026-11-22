//! Host-owned, exact provider routes. Selecting an id never constructs a new
//! endpoint, credential binding, or model from controller/model-supplied data.
use crate::{
    protocol::ModelRequest,
    provider::{CountTokens, Identity, Provider, ProviderError, Reply},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MAX_ROUTES: usize = 1024;

/// The full provider directory the install ships (`models.json`, the same
/// file the TS catalog service reads), with the self-maintained model patches
/// applied. An explicit path wins; otherwise the newest readable snapshot
/// beside the engine binary, in the install directory, or in the user cache
/// is used.
pub(crate) fn models_snapshot() -> Option<serde_json::Value> {
    let mut snapshot = raw_snapshot()?;
    // MIAO_MODELS_PATH selects an exact catalog (tests, air-gapped users), so
    // the self-maintained overlay does not apply there — the same rule the TS
    // catalog service applies.
    if std::env::var_os("MIAO_MODELS_PATH").is_none() {
        apply_overlay(&mut snapshot);
    }
    Some(snapshot)
}

fn raw_snapshot() -> Option<serde_json::Value> {
    // An explicit path selects an exact catalog, even when it is older than
    // the cache; the operator or a test asked for that file.
    if let Some(path) = std::env::var_os("MIAO_MODELS_PATH") {
        return read_snapshot(Path::new(&path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("models.json"));
        }
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".miao/bin/models.json"));
        candidates.push(home.join(".local/share/miao/bin/models.json"));
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        // The copy the TS catalog service keeps and serves after its first
        // fetch. It is usually newer than the installer's shipped snapshot,
        // and it is the only place a freshly released model appears until the
        // next install.
        candidates.push(cache.join("miao/models.json"));
    }
    newest_snapshot(candidates)
}

/// The home directory the product and the TS catalog service share.
///
/// A native Windows binary often has `USERPROFILE` but no `HOME`; a Git Bash
/// `HOME` such as `/c/Users/name` is not a valid native path. Prefer the
/// native variable on Windows and keep `HOME` for Unix.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(home));
        }
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        {
            return Some(PathBuf::from(drive).join(path));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

fn read_snapshot(path: &Path) -> Option<serde_json::Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Pick the newest readable snapshot. The installer's copy and the TS cache
/// can both exist; whichever was written last is the one that knows the
/// current model list.
fn newest_snapshot(candidates: Vec<PathBuf>) -> Option<serde_json::Value> {
    candidates
        .into_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok();
            read_snapshot(&path).map(|snapshot| (modified, snapshot))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, snapshot)| snapshot)
}

/// The model patches miao maintains itself, mirrored from
/// `packages/engine-ts/core/src/models-overlay.ts`. The TS catalog service
/// merges that full table (provider identity, pricing) in memory, but the
/// cache file it writes — the file this engine reads — stays the raw fetch,
/// so an engine decision made from the snapshot alone only ever saw upstream
/// data. That gap is how volcengine/deepseek-v4-1-flash-261017 lost pasted
/// images on every route built from the snapshot: upstream marks the dated id
/// text-only while Ark accepts an `image_url` part for it (checked against
/// the live endpoint). Providers the snapshot does not name stay TS-side:
/// inserting them is plugin identity and transport, not engine routing.
fn apply_overlay(snapshot: &mut serde_json::Value) {
    let Some(entries) = snapshot.as_object_mut() else {
        return;
    };
    let patches = serde_json::json!({
        "zhipuai-coding-plan": {
            "glm-5.3": {
                "cost": { "input": 1.19516, "output": 4.18306, "cache_read": 0.29879, "cache_write": 0 },
            },
            "glm-5.3-flash": {
                "cost": { "input": 0.11952, "output": 0.41831, "cache_read": 0.03436, "cache_write": 0 },
            },
        },
        "volcengine": {
            "deepseek-v4-1-flash-261017": {
                "modalities": { "input": ["text", "image"], "output": ["text"] },
            },
        },
    });
    let Some(patches) = patches.as_object() else {
        return;
    };
    for (provider_id, models) in patches {
        let Some(existing_models) = entries
            .get_mut(provider_id.as_str())
            .and_then(|provider| provider.get_mut("models"))
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let Some(models) = models.as_object() else {
            continue;
        };
        for (model_id, patch) in models {
            match existing_models.get_mut(model_id.as_str()) {
                Some(model) => {
                    if let (Some(model), Some(patch)) = (model.as_object_mut(), patch.as_object()) {
                        for (field, value) in patch {
                            model.insert(field.clone(), value.clone());
                        }
                    }
                }
                None => {
                    existing_models.insert(model_id.clone(), patch.clone());
                }
            }
        }
    }
}

/// The smallest context window each model id declares, across every provider in
/// the shipped snapshot. The same id is often served by many providers with
/// different declared windows; the smallest is the one every caller must assume.
///
/// Built in one pass: the compaction trigger consults the result every turn, so
/// a per-id rescan of the whole document would be quadratic in an eight-thousand
/// model file.
fn index_windows(document: &Value) -> BTreeMap<String, u64> {
    let mut index = BTreeMap::new();
    for provider in document
        .as_object()
        .into_iter()
        .flat_map(|entries| entries.values())
    {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, info) in models {
            let Some(window) = info
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(Value::as_u64)
            else {
                continue;
            };
            index
                .entry(model_id.clone())
                .and_modify(|known: &mut u64| {
                    if window < *known {
                        *known = window;
                    }
                })
                .or_insert(window);
        }
    }
    index
}

fn window_index() -> &'static BTreeMap<String, u64> {
    static INDEX: std::sync::OnceLock<BTreeMap<String, u64>> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        models_snapshot()
            .as_ref()
            .map(index_windows)
            .unwrap_or_default()
    })
}

/// A model id with a trailing date pin (`…-261017`) shares its family's window:
/// the snapshot indexes the unsuffixed id, and a route that pins a dated build
/// must not lose the trigger because of the suffix.
fn family_id(model: &str) -> Option<&str> {
    let (base, tail) = model.rsplit_once('-')?;
    (!tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit())).then_some(base)
}

/// The window one id resolves to in an already-built index: the exact id first,
/// then its undated family id.
fn window_in_index(index: &BTreeMap<String, u64>, model: &str) -> Option<u64> {
    index
        .get(model)
        .copied()
        .or_else(|| family_id(model).and_then(|base| index.get(base).copied()))
}

/// The context window the shipped snapshot declares for one model id, falling
/// back to the undated family id. This is the window the compaction trigger
/// seeds from before any endpoint has refused a request.
pub(crate) fn context_window_for(model: &str) -> Option<u64> {
    window_in_index(window_index(), model)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Info {
    pub route_id: Option<String>,
    pub protocol: String,
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// The operator's name for the account this route spends. Providers can
    /// hold several credentials; the label is what tells them apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Trusted route metadata. Only logical model and opaque credential identities
/// appear in model_catalog replies; endpoints and secrets remain private. `model_key`
/// is a host-declared logical model family, not inferred from route id syntax.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: Option<String>,
    pub model_key: String,
    #[serde(default, rename = "default")]
    pub is_default: bool,
    #[serde(default)]
    pub label: Option<String>,
}

pub struct Catalog {
    primary: Arc<dyn Provider>,
    routes: BTreeMap<String, Arc<dyn Provider>>,
    variants: BTreeMap<(String, String), Arc<dyn Provider>>,
    primary_variants: BTreeMap<String, Arc<dyn Provider>>,
    accounts: BTreeMap<String, Account>,
    primary_route: Option<String>,
}
impl Catalog {
    pub fn new(
        primary: Arc<dyn Provider>,
        entries: Vec<(String, Arc<dyn Provider>)>,
    ) -> Result<Self, ProviderError> {
        if entries.len() > MAX_ROUTES {
            return Err(ProviderError::Stream(
                "model route catalog exceeds 1024 entries".into(),
            ));
        }
        let mut routes = BTreeMap::new();
        for (id, provider) in entries {
            if !valid_id(&id) || provider.identity().is_none() {
                return Err(ProviderError::Stream(
                    "invalid model route catalog entry".into(),
                ));
            }
            if routes.insert(id, provider).is_some() {
                return Err(ProviderError::Stream("duplicate model route id".into()));
            }
        }
        Ok(Self {
            primary,
            routes,
            variants: BTreeMap::new(),
            primary_variants: BTreeMap::new(),
            accounts: BTreeMap::new(),
            primary_route: None,
        })
    }

    pub fn with_accounts(
        mut self,
        entries: Vec<(String, Account)>,
        primary_route: Option<String>,
    ) -> Result<Self, ProviderError> {
        let mut defaults = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for (route, account) in entries {
            if !self.routes.contains_key(&route)
                || account.model_key.is_empty()
                || account.model_key.len() > 512
                || account.model_key.chars().any(char::is_control)
                || account.id.as_ref().is_some_and(|id| {
                    id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                })
                || (account.is_default && !defaults.insert(account.model_key.clone()))
                || (account.id.is_some()
                    && !identities.insert((account.model_key.clone(), account.id.clone())))
                || self.accounts.insert(route, account).is_some()
            {
                return Err(ProviderError::Stream(
                    "invalid or ambiguous account route metadata".into(),
                ));
            }
        }
        if primary_route.as_ref().is_some_and(|route| {
            !self.accounts.contains_key(route)
                || self
                    .routes
                    .get(route)
                    .is_none_or(|provider| provider.identity() != self.primary.identity())
        }) {
            return Err(ProviderError::Stream(
                "default model account route is not registered".into(),
            ));
        }
        self.primary_route = primary_route;
        Ok(self)
    }

    pub fn with_variants(
        mut self,
        entries: Vec<(String, String, Arc<dyn Provider>)>,
        primary: Vec<(String, Arc<dyn Provider>)>,
    ) -> Result<Self, ProviderError> {
        if entries.len() + primary.len() > 8192 {
            return Err(ProviderError::Stream("model variant catalog limit".into()));
        }
        for (route, variant, provider) in entries {
            if !valid_variant(&variant)
                || self
                    .routes
                    .get(&route)
                    .is_none_or(|base| base.identity() != provider.identity())
                || self.variants.insert((route, variant), provider).is_some()
            {
                return Err(ProviderError::Stream(
                    "invalid or duplicate model variant".into(),
                ));
            }
        }
        for (variant, provider) in primary {
            if !valid_variant(&variant)
                || self.primary.identity() != provider.identity()
                || self.primary_variants.insert(variant, provider).is_some()
            {
                return Err(ProviderError::Stream(
                    "invalid or duplicate default model variant".into(),
                ));
            }
        }
        Ok(self)
    }
}

pub(crate) fn valid_variant(variant: &str) -> bool {
    !variant.is_empty() && variant.len() <= 128 && !variant.chars().any(char::is_control)
}

pub(crate) fn valid_id(id: &str) -> bool {
    // `@` is the account slot a per-key route carries
    // (`provider/model@account`, from `native_routes::account_slot`, whose own
    // alphabet is alphanumerics, `-` and `_`). Leaving `@` out rejected every
    // derived per-account route, which failed the whole catalog reload the
    // moment a second key was stored for one provider (#2928's /accounts flow).
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._:/-".contains(c))
}

#[cfg(test)]
mod valid_id_tests {
    use super::valid_id;

    #[test]
    fn a_per_account_route_id_is_valid() {
        assert!(valid_id("mtty/glm-5.3-flash@dingzhigang"));
        assert!(valid_id("volcengine/deepseek-v4.1-flash-free"));
    }

    #[test]
    fn junk_ids_stay_invalid() {
        assert!(!valid_id(""));
        assert!(!valid_id("has space"));
        assert!(!valid_id("newline\n"));
    }
}

#[async_trait]
impl Provider for Catalog {
    fn identity(&self) -> Option<Identity> {
        self.primary.identity()
    }
    fn reply_identity(&self, reply: &Reply) -> Option<Identity> {
        self.primary.reply_identity(reply)
    }
    fn for_model(&self, model: &str) -> Option<Arc<dyn Provider>> {
        self.primary.for_model(model)
    }
    fn for_route(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.routes.get(id).cloned()
    }
    fn for_route_variant(&self, id: Option<&str>, variant: &str) -> Option<Arc<dyn Provider>> {
        match id {
            Some(id) => self
                .variants
                .get(&(id.to_owned(), variant.to_owned()))
                .cloned(),
            None => self.primary_variants.get(variant).cloned(),
        }
    }
    fn route_account(&self, route: &str) -> Option<Account> {
        self.accounts.get(route).cloned()
    }

    fn account_route(
        &self,
        credential: Option<&str>,
        current: Option<&crate::session_model::Binding>,
        identity: &Identity,
    ) -> Result<Option<Info>, ProviderError> {
        if self.accounts.is_empty() {
            return Ok(None);
        }
        let current_route = current
            .and_then(|binding| binding.route_id.as_ref())
            .or_else(|| {
                (self.primary.identity().as_ref() == Some(identity))
                    .then_some(self.primary_route.as_ref())
                    .flatten()
            });
        let family = current_route
            .and_then(|route| self.accounts.get(route))
            .map(|account| account.model_key.as_str());
        // First pass: the route that keeps the model the session is on, under
        // the account that was asked for. "Switch account" is not "switch
        // model", so this pass wins whenever the account serves the family.
        // Two routes of one credential serving the same family are not an
        // operator's ambiguity to resolve: the catalog order is the answer.
        let mut same_family: Option<Info> = None;
        // Second pass: the account's own first route. Reaching it means the
        // account does not serve the model the session is on, so the switch
        // moves the model too — refusing left the operator stuck on a key they
        // had just asked to leave.
        let mut first_served: Option<Info> = None;
        for info in self.model_catalog() {
            let Some(account) = info
                .route_id
                .as_ref()
                .and_then(|route| self.accounts.get(route))
            else {
                continue;
            };
            let served = match credential {
                Some(id) => account.id.as_deref() == Some(id),
                None => account.is_default,
            };
            if !served {
                continue;
            }
            if first_served.is_none() {
                first_served = Some(info.clone());
            }
            let same_model = family.map_or(
                info.protocol == identity.protocol && info.model == identity.model,
                |family| family == account.model_key,
            );
            if same_model && same_family.is_none() {
                // The pinned variant belongs to this model: a route that cannot
                // host it is refused, the way it always was.
                if let Some(variant) = current.and_then(|binding| binding.variant.as_ref()) {
                    if !info.variants.contains(variant) {
                        return Err(ProviderError::Stream(
                            "account route does not support the selected variant".into(),
                        ));
                    }
                }
                same_family = Some(info);
            }
        }
        Ok(same_family.or(first_served))
    }

    fn model_catalog(&self) -> Vec<Info> {
        self.primary
            .identity()
            .into_iter()
            .map(|identity| Info {
                route_id: None,
                protocol: identity.protocol,
                model: identity.model,
                variants: self.primary_variants.keys().cloned().collect(),
                model_key: self
                    .primary_route
                    .as_ref()
                    .and_then(|route| self.accounts.get(route))
                    .map(|account| account.model_key.clone()),
                credential_id: self
                    .primary_route
                    .as_ref()
                    .and_then(|route| self.accounts.get(route))
                    .and_then(|account| account.id.clone()),
                account: self
                    .primary_route
                    .as_ref()
                    .and_then(|route| self.accounts.get(route))
                    .and_then(|account| account.label.clone()),
            })
            .chain(self.routes.iter().filter_map(|(id, provider)| {
                provider.identity().map(|identity| Info {
                    model_key: self
                        .accounts
                        .get(id)
                        .map(|account| account.model_key.clone()),
                    credential_id: self.accounts.get(id).and_then(|account| account.id.clone()),
                    account: self
                        .accounts
                        .get(id)
                        .and_then(|account| account.label.clone()),
                    route_id: Some(id.clone()),
                    protocol: identity.protocol,
                    model: identity.model,
                    variants: self
                        .variants
                        .range((id.clone(), String::new())..)
                        .take_while(|((route, _), _)| route == id)
                        .map(|((_, variant), _)| variant.clone())
                        .collect(),
                })
            }))
            .collect()
    }
    fn protected_resources(&self) -> Vec<PathBuf> {
        self.primary
            .protected_resources()
            .into_iter()
            .chain(
                self.routes
                    .values()
                    .flat_map(|provider| provider.protected_resources()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    async fn count_tokens(&self, request: &ModelRequest) -> CountTokens {
        self.primary.count_tokens(request).await
    }
    fn supports_tool_free_stream(&self) -> bool {
        self.primary.supports_tool_free_stream()
    }
    async fn stream_without_tools(
        &self,
        request: ModelRequest,
        progress: mpsc::Sender<Value>,
        cancel: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        self.primary
            .stream_without_tools(request, progress, cancel)
            .await
    }
    async fn stream(
        &self,
        request: ModelRequest,
        progress: mpsc::Sender<Value>,
        cancel: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        self.primary.stream(request, progress, cancel).await
    }
}

#[cfg(test)]
mod window_tests {
    use super::{apply_overlay, index_windows, newest_snapshot, window_in_index};

    fn document() -> serde_json::Value {
        serde_json::json!({
            "alpha": {"models": {
                "m-big": {"limit": {"context": 200000}},
                "m-small": {"limit": {"context": 64000}},
                "m-none": {}
            }},
            "beta": {"models": {
                "m-big": {"limit": {"context": 100000}},
                "m-big-261017": {"limit": {"context": 50000}}
            }},
            "gamma": {"models": {
                "m-big": {"limit": {}}
            }}
        })
    }

    #[test]
    fn the_index_keeps_the_smallest_window_each_id_declares() {
        let index = index_windows(&document());
        assert_eq!(index.get("m-big"), Some(&100000));
        assert_eq!(index.get("m-small"), Some(&64000));
        assert!(
            !index.contains_key("m-none"),
            "an entry that declares no window is not a window"
        );
    }

    #[test]
    fn a_dated_pin_falls_back_to_its_family_window() {
        let index = index_windows(&document());
        // An exact dated id wins over the family.
        assert_eq!(window_in_index(&index, "m-big-261017"), Some(50000));
        // A dated id the snapshot never indexed still arms the trigger with the
        // family window instead of leaving it off until an endpoint refuses.
        assert_eq!(window_in_index(&index, "m-small-261017"), Some(64000));
        assert_eq!(window_in_index(&index, "m-unknown-261017"), None);
        assert_eq!(window_in_index(&index, "m-small"), Some(64000));
    }

    #[test]
    fn the_overlay_restores_image_input_for_the_dated_volcengine_id() {
        let mut snapshot = serde_json::json!({
            "volcengine": {
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "deepseek-v4-1-flash-261017": {
                        "modalities": { "input": ["text"], "output": ["text"] },
                    },
                },
            },
        });
        apply_overlay(&mut snapshot);
        assert_eq!(
            snapshot["volcengine"]["models"]["deepseek-v4-1-flash-261017"]["modalities"]["input"],
            serde_json::json!(["text", "image"]),
        );
    }

    #[test]
    fn the_overlay_merges_field_wise_without_erasing_the_catalog_entry() {
        let mut snapshot = serde_json::json!({
            "volcengine": {
                "models": {
                    "deepseek-v4-1-flash-261017": {
                        "name": "DeepSeek V4.1 Flash 1017",
                        "limit": { "context": 1048576 },
                        "modalities": { "input": ["text"], "output": ["text"] },
                    },
                },
            },
        });
        apply_overlay(&mut snapshot);
        let model = &snapshot["volcengine"]["models"]["deepseek-v4-1-flash-261017"];
        assert_eq!(model["name"], serde_json::json!("DeepSeek V4.1 Flash 1017"));
        assert_eq!(model["limit"]["context"], serde_json::json!(1048576));
        assert_eq!(
            model["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(model["modalities"]["output"], serde_json::json!(["text"]));
    }

    #[test]
    fn the_overlay_adds_a_model_the_catalog_misses() {
        let mut snapshot = serde_json::json!({ "volcengine": { "models": {} } });
        apply_overlay(&mut snapshot);
        assert_eq!(
            snapshot["volcengine"]["models"]["deepseek-v4-1-flash-261017"]["modalities"]["input"],
            serde_json::json!(["text", "image"]),
        );
    }

    #[test]
    fn the_overlay_skips_providers_the_snapshot_does_not_name() {
        let mut snapshot = serde_json::json!({ "zhipuai": { "models": {} } });
        apply_overlay(&mut snapshot);
        assert!(snapshot.get("volcengine").is_none());
        let mut empty = serde_json::json!({});
        apply_overlay(&mut empty);
        assert_eq!(empty, serde_json::json!({}));
    }

    #[test]
    fn the_newest_snapshot_wins_over_the_shipped_copy() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old.json");
        let new = directory.path().join("new.json");
        std::fs::write(&old, r#"{"old":{}}"#).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::write(&new, r#"{"new":{}}"#).unwrap();
        let picked = newest_snapshot(vec![old, new]).expect("snapshot");
        assert!(picked.get("new").is_some(), "{picked}");
    }
}
