//! A suggested next prompt is one one-shot call on the Session's own route.
//!
//! It never becomes a turn: nothing is admitted, and a model that answers
//! nothing leaves the composer without a suggestion rather than failing it.

use miao_engine::{
    permission::{Config, Policy},
    protocol::{Command, ModelRequest},
    provider::{Identity, Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Suggests {
    seen: Arc<Mutex<Option<ModelRequest>>>,
    answer: Value,
}

#[async_trait::async_trait]
impl Provider for Suggests {
    fn identity(&self) -> Option<Identity> {
        Some(Identity {
            protocol: "openai-chat".into(),
            model: "base".into(),
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::Sender<Value>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Reply, ProviderError> {
        *self.seen.lock().unwrap() = Some(request);
        Ok(Reply {
            content: self.answer.clone(),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

async fn host(
    answer: Value,
) -> (
    miao_engine::host::Host,
    Arc<Mutex<Option<ModelRequest>>>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let provider: Arc<dyn Provider> = Arc::new(Suggests {
        seen: Arc::clone(&seen),
        answer,
    });
    let runtime = Runtime::with_policy(
        Store::open(dir.path().join("engine.db")).await.unwrap(),
        provider,
        Tools::new(dir.path()).await.unwrap(),
        Policy::new(Config::default()).unwrap(),
    )
    .await
    .unwrap();
    (miao_engine::host::Host::new(runtime), seen, dir)
}

async fn initialize(host: &mut miao_engine::host::Host, session: &str) {
    host.dispatch(Command::Initialize {
        session_id: session.into(),
        title: "t".into(),
        protocol: miao_engine::PROTOCOL,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_prediction_is_one_call_shown_the_draft_and_reduced_to_one_line() {
    let (mut host, seen, _dir) = host(json!([{
        "type": "text",
        "text": "```\nadd the missing test\n```",
    }]))
    .await;
    initialize(&mut host, "s1").await;

    let answer = host
        .dispatch(Command::Predict {
            session_id: "s1".into(),
            draft: Some("finish the ".into()),
        })
        .await
        .unwrap();

    assert_eq!(answer["prediction"], "add the missing test");
    let request = seen.lock().unwrap().clone().expect("the model was asked");
    assert!(request.tools.is_empty(), "a suggestion never needs tools");
    let shown = request.messages[0].content.to_string();
    assert!(
        shown.contains("draft: finish the"),
        "the model sees what was typed: {shown}"
    );
}

#[tokio::test]
async fn nothing_to_predict_is_no_suggestion_rather_than_an_error() {
    let (mut host, seen, _dir) = host(json!([{ "type": "text", "text": "```\n\n```" }])).await;
    initialize(&mut host, "s1").await;

    // An empty Session with no draft has nothing to show the model.
    let answer = host
        .dispatch(Command::Predict {
            session_id: "s1".into(),
            draft: None,
        })
        .await
        .unwrap();
    assert_eq!(answer["prediction"], Value::Null);
    assert!(
        seen.lock().unwrap().is_none(),
        "no call is made without context"
    );

    // With a draft the call happens, and an answer that says nothing is None.
    let answer = host
        .dispatch(Command::Predict {
            session_id: "s1".into(),
            draft: Some("finish the ".into()),
        })
        .await
        .unwrap();
    assert_eq!(answer["prediction"], Value::Null);
}
