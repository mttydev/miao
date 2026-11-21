//! A first start with nothing configured is onboarding, not an error: the
//! engine serves the provider and model catalogs the picker drives, and the
//! key stored from inside the TUI becomes a real route the next start derives.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// The catalog schema, which also carries the endpoint and transport a stored
/// key needs. Only `zhipuai-coding-plan` speaks the OpenAI-compatible wire the
/// snapshot half of the route derivation accepts, so it is the provider a key
/// can turn into a route.
const SNAPSHOT: &str = r#"{
  "zhipuai-coding-plan": {
    "id": "zhipuai-coding-plan",
    "name": "Zhipu",
    "npm": "@ai-sdk/openai-compatible",
    "api": "https://open.bigmodel.cn/api/paas/v4",
    "models": {
      "glm-5.3-flash": {
        "id": "glm-5.3-flash",
        "modalities": {"input": ["text"], "output": ["text"]},
        "limit": {"context": 128000, "output": 8192}
      }
    }
  },
  "anthropic": {
    "id": "anthropic",
    "name": "Anthropic",
    "npm": "@ai-sdk/anthropic",
    "api": "https://api.anthropic.com",
    "models": {
      "claude-sonnet": {
        "id": "claude-sonnet",
        "modalities": {"input": ["text"], "output": ["text"]}
      }
    }
  }
}"#;

struct Layout {
    directory: tempfile::TempDir,
}

impl Layout {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path();
        for name in ["ws", "config", "data", "storage"] {
            std::fs::create_dir_all(path.join(name)).expect("layout dir");
        }
        std::fs::write(path.join("models.json"), SNAPSHOT).expect("snapshot file");
        Self { directory }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn data_home(&self) -> PathBuf {
        self.join("data")
    }

    /// The credential store a plain start reads, with one live key. Its schema
    /// mirrors the product database the installation writes.
    fn store_key(&self, integration: &str) {
        let path = self.data_home().join("miao.db");
        let connection = Connection::open(&path).expect("credential db");
        connection
            .execute_batch(&format!(
                r#"
                CREATE TABLE credential (
                  id text PRIMARY KEY,
                  integration_id text,
                  label text NOT NULL,
                  value text NOT NULL,
                  connector_id text,
                  method_id text,
                  active integer,
                  time_created integer NOT NULL,
                  time_updated integer NOT NULL
                );
                INSERT INTO credential VALUES (
                  'cred-one', '{integration}', 'default',
                  '{{"type":"key","key":"sk-test"}}', NULL, NULL, 1, 1, 1
                );
                "#
            ))
            .expect("credential schema");
    }

    /// Run one start and answer every reply, in order. No model is ever named:
    /// that is the state a fresh install is in.
    fn serve(&self, requests: &[Value]) -> (Vec<Value>, Output) {
        let mut child = self.spawn();
        {
            let mut stdin = child.stdin.take().expect("stdin");
            for request in requests {
                writeln!(stdin, "{request}").expect("request");
            }
        }
        let output = child.wait_with_output().expect("engine exit");
        let replies = String::from_utf8(output.stdout.clone())
            .expect("utf-8 replies")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json reply"))
            .collect();
        (replies, output)
    }

    fn spawn(&self) -> std::process::Child {
        let child = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
            .args(["serve", "--db"])
            .arg(self.join("engine.db"))
            .args(["--workspace"])
            .arg(self.join("ws"))
            .args(["--config-dir"])
            .arg(self.join("config"))
            .env("HOME", self.directory.path())
            .env("MIAO_DATA_HOME", self.data_home())
            .env("MIAO_ENGINE_STORAGE", self.join("storage"))
            .env("MIAO_MODELS_PATH", self.join("models.json"))
            // A host that named a route would make this a modeled start; the
            // first-run state is one where nothing named one.
            .env_remove("MIAO_ENGINE_MODEL_ROUTES_CONFIG")
            .env_remove("MIAO_ENGINE_MODEL_ROUTES")
            .env_remove("MIAO_ENGINE_PROVIDER")
            .env_remove("MIAO_ENGINE_ENDPOINT")
            .env_remove("MIAO_ENGINE_CREDENTIAL_DB")
            .env_remove("MIAO_ENGINE_CREDENTIAL_ID")
            .env_remove("MIAO_ENGINE_CREDENTIAL_INTEGRATION")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("engine start");
        child
    }
}

fn result(replies: &[Value], id: u64) -> Value {
    replies
        .iter()
        .find(|reply| reply["id"] == id)
        .unwrap_or_else(|| panic!("no reply {id} in {replies:?}"))["result"]
        .clone()
}

/// One request/reply exchange on a running engine.
fn ask(
    stdin: &mut std::process::ChildStdin,
    reader: &mut std::io::BufReader<std::process::ChildStdout>,
    request: Value,
) -> Value {
    let id = request["id"].clone();
    writeln!(stdin, "{request}").expect("request");
    stdin.flush().expect("flush");
    loop {
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).expect("reply");
        let reply: Value = serde_json::from_str(&line).expect("json reply");
        // A reload also emits an unsolicited notification, which carries no
        // request id; it must not be mistaken for the answer.
        if reply["id"] == id {
            return reply;
        }
    }
}

#[test]
fn a_start_without_a_model_serves_the_catalogs() {
    let layout = Layout::new();
    let (replies, output) = layout.serve(&[
        json!({"id":1,"method":"provider_catalog"}),
        json!({"id":2,"method":"model_catalog"}),
        json!({"id":3,"method":"shutdown"}),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let providers = result(&replies, 1)["providers"]
        .as_array()
        .expect("provider rows")
        .clone();
    // Every provider the install ships stays visible, and none claims to be
    // configured: the operator can see the whole road before holding a key.
    assert!(providers.iter().any(|row| row["id"] == "anthropic"));
    assert!(providers
        .iter()
        .any(|row| row["id"] == "zhipuai-coding-plan"));
    assert!(providers
        .iter()
        .all(|row| row["configured"] == false && row["accounts"] == 0));
    // Nothing is pickable because nothing is keyed.
    assert_eq!(result(&replies, 2), json!([]));
}

#[test]
fn a_stored_key_becomes_a_served_route_without_a_model() {
    let layout = Layout::new();
    layout.store_key("zhipuai-coding-plan");
    let (replies, output) = layout.serve(&[
        json!({"id":1,"method":"provider_catalog"}),
        json!({"id":2,"method":"model_catalog"}),
        json!({"id":3,"method":"shutdown"}),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let models = result(&replies, 2).as_array().expect("models").clone();
    // The start derived a route from the key and the shipped snapshot, so the
    // picker names the model with the route a selection binds to.
    let picked = models
        .iter()
        .find(|model| model["route_id"] == "zhipuai-coding-plan/glm-5.3-flash")
        .unwrap_or_else(|| panic!("no route for the stored key: {models:?}"));
    assert_eq!(picked["model"], "glm-5.3-flash");
    assert_eq!(picked["protocol"], "openai-chat");
    let providers = result(&replies, 1)["providers"]
        .as_array()
        .expect("provider rows")
        .clone();
    let zhipu = providers
        .iter()
        .find(|row| row["id"] == "zhipuai-coding-plan")
        .expect("provider row");
    assert_eq!(zhipu["configured"], true);
    assert_eq!(zhipu["accounts"], 1);
    // A provider without a key stays a needs-auth row.
    let anthropic = providers
        .iter()
        .find(|row| row["id"] == "anthropic")
        .expect("provider row");
    assert_eq!(anthropic["configured"], false);
}

/// The in-process flow: the TUI cannot respawn its engine, so a key stored
/// while it runs has to become a route through `config_reload`, not a restart.
#[test]
fn a_key_stored_while_the_engine_runs_becomes_a_route_on_reload() {
    let layout = Layout::new();
    let mut child = layout.spawn();
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let mut stderr = child.stderr.take().expect("stderr");
    let catalog = ask(
        &mut stdin,
        &mut reader,
        json!({"id":1,"method":"model_catalog"}),
    );
    assert_eq!(catalog["result"], json!([]));
    // The key arrives from inside the running TUI, after startup.
    layout.store_key("zhipuai-coding-plan");
    let report = ask(
        &mut stdin,
        &mut reader,
        json!({"id":2,"method":"config_reload"}),
    );
    let report = &report["result"];
    let unsupported = report["unsupported"].as_array().expect("unsupported");
    assert!(
        !unsupported
            .iter()
            .any(|section| section.as_str() == Some("models")),
        "the derived origin supports models: {report}"
    );
    assert!(
        report["changed"]
            .as_array()
            .expect("changed")
            .iter()
            .any(|section| section.as_str() == Some("models")),
        "the stored key changed the model section: {report}"
    );
    let catalog = ask(
        &mut stdin,
        &mut reader,
        json!({"id":3,"method":"model_catalog"}),
    );
    let models = catalog["result"].as_array().expect("models").clone();
    assert!(
        models.iter().any(|model| model["model"] == "glm-5.3-flash"
            && model["route_id"] == "zhipuai-coding-plan/glm-5.3-flash"),
        "the reloaded catalog serves the stored key: {models:?}"
    );
    ask(&mut stdin, &mut reader, json!({"id":4,"method":"shutdown"}));
    drop(stdin);
    drop(reader);
    let status = child.wait().expect("engine exit");
    let mut message = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut message).expect("stderr");
    assert!(status.success(), "{message}");
}
