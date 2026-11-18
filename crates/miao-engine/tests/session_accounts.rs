use async_trait::async_trait;
use miao_engine::{
    host::Host,
    model_catalog::{Account, Catalog},
    protocol::{Command, Error, ModelRequest},
    provider::{Identity, Provider, ProviderError, Reply},
    runtime::Runtime,
    session_model::Binding,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Answer;
#[async_trait]
impl Provider for Answer {
    fn identity(&self) -> Option<Identity> {
        Some(Identity {
            protocol: "fixture".into(),
            model: "wire-shared".into(),
        })
    }
    async fn stream(
        &self,
        _: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: json!([{"type":"text","text":"done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}
fn binding(route: &str) -> Binding {
    Binding {
        protocol: "fixture".into(),
        model: "wire-shared".into(),
        route_id: Some(route.into()),
        variant: Some("high".into()),
    }
}
fn account(id: &str, family: &str, is_default: bool) -> Account {
    Account {
        id: Some(id.into()),
        model_key: family.into(),
        is_default,
        label: None,
    }
}
fn catalog() -> Arc<dyn Provider> {
    let primary: Arc<dyn Provider> = Arc::new(Answer);
    let routes = ["a", "b", "c", "other"]
        .into_iter()
        .map(|id| (id.into(), primary.clone()))
        .collect();
    let variants = ["a", "b", "other"]
        .into_iter()
        .map(|id| (id.into(), "high".into(), primary.clone()))
        .collect();
    Arc::new(
        Catalog::new(primary, routes)
            .unwrap()
            .with_variants(variants, vec![])
            .unwrap()
            .with_accounts(
                vec![
                    ("a".into(), account("credential-a", "host/shared", true)),
                    ("b".into(), account("credential-b", "host/shared", false)),
                    ("c".into(), account("credential-c", "host/shared", false)),
                    (
                        "other".into(),
                        account("credential-b", "other-provider/shared", true),
                    ),
                ],
                Some("a".into()),
            )
            .unwrap(),
    )
}

#[tokio::test]
async fn account_selection_retains_model_family_variant_and_is_session_local() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let provider = catalog();
    // Public catalogs carry display identities, never endpoint or secret data.
    let public = serde_json::to_value(provider.model_catalog()).unwrap();
    assert_eq!(public[0]["model_key"], "host/shared");
    assert_eq!(public[0]["credential_id"], "credential-a");
    assert!(public
        .as_array()
        .unwrap()
        .iter()
        .any(
            |route| route["credential_id"] == "credential-b" && route["model_key"] == "host/shared"
        ));
    assert!(!public.to_string().contains("127.0.0.1"));
    let runtime = Runtime::new(
        store.clone(),
        provider,
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    for id in ["s", "unrelated"] {
        store
            .initialize(
                id,
                "fixture",
                std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
            )
            .await
            .unwrap();
    }
    runtime.select_model("s", Some(binding("a"))).await.unwrap();
    let mut host = Host::new(runtime.clone());
    let selected = host.dispatch(serde_json::from_value::<Command>(json!({"method":"select_account","params":{"session_id":"s","credential_id":"credential-b"}})).unwrap()).await.unwrap();
    assert_eq!(selected["binding"]["route_id"], "b");
    assert_eq!(selected["binding"]["variant"], "high");
    assert_eq!(selected["account"]["credential_id"], "credential-b");
    // A model picker/variant update addresses the default model route, but
    // must not silently clear the Session's explicit account pin.
    runtime.select_model("s", Some(binding("a"))).await.unwrap();
    assert_eq!(
        store
            .model_selection("s")
            .await
            .unwrap()
            .unwrap()
            .route_id
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        store.snapshot("s").await.unwrap()["account_selection"]["credential_id"],
        "credential-b"
    );
    let cursor = store.snapshot("s").await.unwrap()["cursor"].clone();
    assert_eq!(
        runtime
            .select_account("s", Some("credential-b".into()))
            .await
            .unwrap()["changed"],
        false
    );
    assert_eq!(store.snapshot("s").await.unwrap()["cursor"], cursor);
    assert!(store.model_selection("unrelated").await.unwrap().is_none());
    // A fork carries the account intent with the exact route binding.
    store.fork("s", "child", None).await.unwrap();
    assert_eq!(
        store.snapshot("child").await.unwrap()["account_selection"]["credential_id"],
        "credential-b"
    );
    // Clearing the pin returns to this logical model's default account, not the
    // process's unrelated default model, and retains the explicit variant.
    let cleared = runtime.select_account("s", None).await.unwrap();
    assert_eq!(cleared["binding"]["route_id"], "a");
    assert_eq!(cleared["binding"]["variant"], "high");
    assert!(cleared["account"]["credential_id"].is_null());
    runtime.shutdown().await;
}

/// A model pick takes the account with it. Keeping a pin that no longer serves
/// the picked model let `resolve` answer with a model that pin's account
/// serves, so the picker looked like it kept the old account and ignored the
/// pick.
#[tokio::test]
async fn a_model_pick_moves_a_pin_that_does_not_serve_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let provider: Arc<dyn Provider> = Arc::new(Answer);
    let catalog = Arc::new(
        Catalog::new(
            provider.clone(),
            ["a", "b", "c"]
                .into_iter()
                .map(|id| (id.into(), provider.clone()))
                .collect(),
        )
        .unwrap()
        .with_accounts(
            vec![
                ("a".into(), account("credential-a", "host/first", true)),
                ("b".into(), account("credential-b", "host/second", false)),
                ("c".into(), account("credential-c", "host/first", false)),
            ],
            Some("a".into()),
        )
        .unwrap(),
    );
    let runtime = Runtime::new(
        store.clone(),
        catalog,
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    for id in ["s", "t"] {
        store
            .initialize(
                id,
                "fixture",
                std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
            )
            .await
            .unwrap();
    }
    let plain = |route: &str| Binding {
        protocol: "fixture".into(),
        model: "wire-shared".into(),
        route_id: Some(route.into()),
        variant: None,
    };
    for id in ["s", "t"] {
        runtime.select_model(id, Some(plain("a"))).await.unwrap();
        // credential-b serves another family, so the switch lands on its route.
        assert_eq!(
            runtime
                .select_account(id, Some("credential-b".into()))
                .await
                .unwrap()["binding"]["route_id"],
            "b"
        );
    }
    // The pick moves the pin to the account that serves the picked family.
    runtime.select_model("s", Some(plain("c"))).await.unwrap();
    assert_eq!(
        store
            .model_selection("s")
            .await
            .unwrap()
            .unwrap()
            .route_id
            .as_deref(),
        Some("c")
    );
    assert_eq!(
        store.snapshot("s").await.unwrap()["account_selection"]["credential_id"],
        "credential-c"
    );
    // A pick on the default route carries no pin: the stale pin is cleared, so
    // turns run the picked model instead of the old account's route.
    runtime.select_model("t", Some(plain("a"))).await.unwrap();
    assert_eq!(
        store
            .model_selection("t")
            .await
            .unwrap()
            .unwrap()
            .route_id
            .as_deref(),
        Some("a")
    );
    assert!(store.snapshot("t").await.unwrap()["account_selection"].is_null());
    runtime.shutdown().await;
}

#[tokio::test]
async fn missing_accounts_or_variants_do_not_mutate_the_selection() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        catalog(),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    runtime.select_model("s", Some(binding("a"))).await.unwrap();
    let before = store.snapshot("s").await.unwrap();
    for id in ["unknown", "credential-c", ""] {
        assert!(matches!(
            runtime.select_account("s", Some(id.into())).await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(store.snapshot("s").await.unwrap(), before);
    }
    runtime.shutdown().await;
}

#[test]
fn duplicate_defaults_and_account_identities_are_rejected() {
    let provider: Arc<dyn Provider> = Arc::new(Answer);
    let entries = vec![
        ("a".into(), provider.clone()),
        ("b".into(), provider.clone()),
    ];
    assert!(Catalog::new(provider.clone(), entries.clone())
        .unwrap()
        .with_accounts(
            vec![
                ("a".into(), account("a", "same", true)),
                ("b".into(), account("b", "same", true))
            ],
            Some("a".into())
        )
        .is_err());
    assert!(Catalog::new(provider, entries)
        .unwrap()
        .with_accounts(
            vec![
                ("a".into(), account("same-id", "same", true)),
                ("b".into(), account("same-id", "same", false))
            ],
            Some("a".into())
        )
        .is_err());
}

/// "Switch account" is not "switch model": when the account does serve the
/// model the session is on, the family is kept. When it does not — a second
/// key of the same provider that only serves other models — the switch lands
/// on that account's own route instead of refusing, and the old model's
/// variant does not follow across families.
#[tokio::test]
async fn a_switch_to_an_account_that_serves_another_family_lands_on_its_route() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        catalog(),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    // The model arrives as an agent default, the way an operator's configured
    // agent carries it: the account switch must not promote it into a pin.
    let agent = |name: &str, route: &str| {
        serde_json::from_value(json!({"name":name,"model":binding(route)})).unwrap()
    };
    runtime
        .set_agent("s", agent("guide", "other"))
        .await
        .unwrap();
    let switched = runtime
        .select_account("s", Some("credential-c".into()))
        .await
        .unwrap();
    assert_eq!(
        switched["binding"]["route_id"], "c",
        "the account's own route wins over the family it does not serve"
    );
    assert!(
        switched["binding"]["variant"].is_null(),
        "the old model's variant does not follow across families: {}",
        switched["binding"]
    );
    assert_eq!(switched["account"]["credential_id"], "credential-c");
    // The move is account intent, never an explicit model pin.
    assert!(store.model_selection("s").await.unwrap().is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn encrypted_history_cannot_move_to_another_account_route() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        catalog(),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    runtime.select_model("s", Some(binding("a"))).await.unwrap();
    store.message("s","assistant",json!([{"type":"provider_opaque","provider":"fixture","model":"wire-shared","_miao_route_id":"a","item":{"type":"reasoning","encrypted_content":"private"}}])).await.unwrap();
    let before = store.snapshot("s").await.unwrap();
    assert!(runtime
        .select_account("s", Some("credential-b".into()))
        .await
        .is_err());
    assert_eq!(store.snapshot("s").await.unwrap(), before);
    runtime.shutdown().await;
}

#[tokio::test]
async fn account_changes_do_not_promote_agent_defaults_into_explicit_model_pins() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        catalog(),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    let agent = |name: &str, route: &str| {
        serde_json::from_value(json!({"name":name,"model":binding(route)})).unwrap()
    };
    runtime.set_agent("s", agent("first", "a")).await.unwrap();
    assert_eq!(
        runtime
            .select_account("s", Some("credential-b".into()))
            .await
            .unwrap()["binding"]["route_id"],
        "b"
    );
    assert!(store.model_selection("s").await.unwrap().is_none());
    runtime
        .set_agent("s", agent("second", "other"))
        .await
        .unwrap();
    assert_eq!(
        runtime
            .select_account("s", Some("credential-b".into()))
            .await
            .unwrap()["binding"]["route_id"],
        "other"
    );
    assert!(store.model_selection("s").await.unwrap().is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn a_fork_uses_the_account_at_its_requested_history_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        catalog(),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    runtime.select_model("s", Some(binding("a"))).await.unwrap();
    runtime
        .select_account("s", Some("credential-a".into()))
        .await
        .unwrap();
    let boundary = store
        .message("s", "assistant", json!([{"type":"text","text":"done"}]))
        .await
        .unwrap()
        .seq;
    runtime
        .select_account("s", Some("credential-b".into()))
        .await
        .unwrap();
    store.fork("s", "past", Some(boundary)).await.unwrap();
    store.fork("s", "now", None).await.unwrap();
    assert_eq!(
        store
            .account_selection("past")
            .await
            .unwrap()
            .unwrap()
            .credential_id
            .as_deref(),
        Some("credential-a")
    );
    assert_eq!(
        store
            .account_selection("now")
            .await
            .unwrap()
            .unwrap()
            .credential_id
            .as_deref(),
        Some("credential-b")
    );
    runtime.shutdown().await;
}
