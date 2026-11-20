//! Report the Session's run state to mtty, the terminal that started us.
//!
//! mtty badges a tab from the `agent.state` map: working, waiting for the
//! operator, or finished, and it routes queued prompts on those transitions.
//! The JavaScript client reported through `mtty-cli state`; the native TUI
//! replaced it without that report, so panes stayed blank. This is the same
//! contract, fed from the phase the TUI already computes for its own title.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::pane::FooterWireSeg;

/// The states mtty understands (`agent.state.set`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Processing,
    Awaiting,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Processing => "processing",
            State::Awaiting => "awaiting",
        }
    }

    /// What a Session's run looks like from outside: a blocking approval or
    /// question outranks a running turn, and no turn at all is idle.
    pub fn of(blocking: bool, running: bool) -> Self {
        match (blocking, running) {
            (true, _) => State::Awaiting,
            (false, true) => State::Processing,
            (false, false) => State::Idle,
        }
    }
}

/// The Runtime this pane owns, in the form mtty's private pipe expects:
/// `miao runtime access` looks for the registration of exactly this identity
/// beside this storage, and refuses to fall back to another local Runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeContext {
    pub runtime_id: String,
    pub storage: String,
}

/// The pane mtty spawned us in and the CLI that reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    pane: String,
    exe: String,
}

impl Target {
    /// `None` outside mtty, where there is nothing to report to.
    fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read the environment through `lookup` so the naming rules are testable.
    /// The `miaotty` names are the pre-rename ones an older host still sets.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let named = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let modern = named("MTTY_PANE_ID");
        let pane = modern.clone().or_else(|| named("MIAOTTY_PANE_ID"))?;
        let exe = named("MTTY_CLI")
            .or_else(|| named("MIAOTTY_CLI"))
            .unwrap_or_else(|| match modern {
                Some(_) => "mtty-cli".to_string(),
                None => "miaotty-cli".to_string(),
            });
        Some(Target { pane, exe })
    }
}

/// The `mtty-cli` arguments for one report.
fn arguments(target: &Target, state: State, session: Option<&str>) -> Vec<String> {
    let mut args = [
        "state",
        "miao",
        "--state",
        state.name(),
        "--pane",
        target.pane.as_str(),
    ]
    .map(str::to_string)
    .to_vec();
    if let Some(session) = session.filter(|session| !session.is_empty()) {
        args.push("--session".to_string());
        args.push(session.to_string());
    }
    // Every update carries the context, including a null one: mtty replaces
    // the pane's entry, so omitting the field would drop the Runtime this
    // client published on the previous state change.
    args.push("--runtime-context".to_string());
    args.push("-".to_string());
    args
}

fn context_json(context: Option<&RuntimeContext>) -> serde_json::Value {
    match context {
        Some(context) => serde_json::json!({
            "kind": "owned",
            "runtimeID": context.runtime_id,
            "storage": context.storage,
        }),
        None => serde_json::Value::Null,
    }
}

/// Whether the running host merges this pane's strip into its own footer.
/// Taking the row over only pays when the host answers with synthesized
/// presses; the capability rides a variable an older host never sets, so a
/// new TUI under one keeps painting its own strip. A pane env is required
/// too: without one there is no one to report to, and a hidden strip with
/// no host row would leave the facts nowhere.
pub fn host_footer_active() -> bool {
    active_from_lookup(|name| std::env::var(name).ok())
}

fn active_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> bool {
    flag_from_lookup(&lookup) && Target::from_lookup(&lookup).is_some()
}

fn flag_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> bool {
    ["MTTY_HOST_FOOTER", "MIAOTTY_HOST_FOOTER"]
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.is_empty()))
}

/// What one footer report carries: the facts the host merges into its chrome,
/// and the build they belong to.
#[derive(Serialize)]
struct FooterReport<'a> {
    v: u8,
    visible: bool,
    version: &'a str,
    segs: &'a [FooterWireSeg],
}

fn footer_json(visible: bool, segs: &[FooterWireSeg]) -> String {
    serde_json::to_string(&FooterReport {
        v: 1,
        visible,
        version: crate::VERSION,
        segs,
    })
    .unwrap_or_else(|_| r#"{"v":1,"visible":false,"version":"","segs":[]}"#.to_string())
}

/// The report a footer change needs, or `None` when the host already shows
/// exactly this. Hiding reports once, on the visible-to-hidden transition; a
/// host that has never seen a footer needs no clearing.
fn footer_update(last: &Option<String>, wire: Option<&[FooterWireSeg]>) -> Option<String> {
    let json = match wire {
        Some(segs) => footer_json(true, segs),
        None => last.as_ref().map(|_| footer_json(false, &[]))?,
    };
    (last.as_deref() != Some(json.as_str())).then_some(json)
}

/// Forwards state changes to mtty, once each.
#[derive(Debug, Default)]
pub struct Reporter {
    target: Option<Target>,
    session: Option<String>,
    context: Option<RuntimeContext>,
    last: Option<State>,
    /// The footer report the host last acknowledged, so a strip that did not
    /// change costs nothing per frame.
    last_footer: Option<String>,
    host_footer: bool,
}

impl Reporter {
    pub fn from_env() -> Self {
        Self {
            target: Target::from_env(),
            session: None,
            context: None,
            last: None,
            last_footer: None,
            host_footer: host_footer_active(),
        }
    }

    /// Whether the host takes this pane's strip over. When it does, the TUI
    /// stops painting the facts row and reports the same facts to the host
    /// instead; the painted hits stay, so a synthesized press from the host
    /// lands on the same handler a press on the strip would.
    pub fn host_footer(&self) -> bool {
        self.host_footer
    }

    /// Publish the strip facts the host's footer should show now. Called once
    /// per paint; a strip that did not change sends nothing.
    pub fn sync_footer(&mut self, wire: Option<Vec<FooterWireSeg>>) {
        if !self.host_footer || self.target.is_none() {
            return;
        }
        let Some(footer) = footer_update(&self.last_footer, wire.as_deref()) else {
            return;
        };
        self.last_footer = Some(footer.clone());
        self.send(self.last.unwrap_or(State::Idle), Some(&footer));
    }

    pub fn set_session(&mut self, session: Option<&str>) {
        self.session = session
            .filter(|session| !session.is_empty())
            .map(str::to_string);
    }

    /// Report `state` when it changed since the last report. Outside mtty this
    /// is one environment lookup at construction and nothing per frame.
    pub fn update(&mut self, state: State) {
        if self.last == Some(state) {
            return;
        }
        self.last = Some(state);
        self.send(state, self.footer_rider().as_deref());
    }

    /// Publish or clear the owned Runtime beside the run state. The report
    /// goes out even when the state did not change, because mtty replaces the
    /// pane's whole entry and a stale context would point at a dead host.
    pub fn set_runtime_context(&mut self, context: Option<RuntimeContext>) {
        self.context = context;
        self.send(
            self.last.unwrap_or(State::Idle),
            self.footer_rider().as_deref(),
        );
    }

    /// The cached footer attached to every report. mtty replaces the pane's
    /// whole entry on each write, so a state change sent without the footer
    /// would silently drop the facts the host is showing until the strip
    /// itself next changed.
    fn footer_rider(&self) -> Option<String> {
        match self.host_footer {
            true => self.last_footer.clone(),
            false => None,
        }
    }

    /// The TUI is leaving: an idle report stops the tab claiming a run that
    /// ended with this process, and a cleared context stops the next client
    /// from attesting a Runtime this one no longer hosts.
    pub fn clear(&mut self) {
        self.context = None;
        self.last = None;
        self.update(State::Idle);
    }

    fn send(&self, state: State, footer: Option<&str>) {
        let Some(target) = &self.target else {
            return;
        };
        let mut command = Command::new(&target.exe);
        command.args(arguments(target, state, self.session.as_deref()));
        if let Some(footer) = footer {
            // The JSON rides an argv slot, not a file: the spawn passes
            // arguments directly to the executable, so no shell ever quotes
            // it, and an older CLI simply ignores the unknown flag.
            command.args(["--footer", footer]);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(context_json(self.context.as_ref()).to_string().as_bytes());
        }
        // Reap off the UI thread: a long Session changes state many times and
        // must not leave a zombie per turn.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{active_from_lookup, arguments, context_json, flag_from_lookup, footer_json};
    use super::{footer_update, Reporter, RuntimeContext, State, Target};

    #[test]
    fn state_follows_the_blocking_and_run_signals() {
        assert_eq!(
            State::of(true, true),
            State::Awaiting,
            "a question outranks a run"
        );
        assert_eq!(State::of(true, false), State::Awaiting);
        assert_eq!(State::of(false, true), State::Processing);
        assert_eq!(State::of(false, false), State::Idle);
        assert_eq!(State::Awaiting.name(), "awaiting");
    }

    #[test]
    fn target_prefers_the_mtty_names_and_falls_back() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let target = Target::from_lookup(env(&[
            ("MTTY_PANE_ID", "pane-1"),
            ("MTTY_CLI", "/opt/mtty-cli"),
        ]))
        .expect("inside mtty");
        assert_eq!(target.pane, "pane-1");
        assert_eq!(target.exe, "/opt/mtty-cli");

        // An older host sets only the pre-rename names.
        let target = Target::from_lookup(env(&[("MIAOTTY_PANE_ID", "old")])).expect("old host");
        assert_eq!(target.exe, "miaotty-cli");

        // A modern pane without MTTY_CLI uses the installer's name.
        let target = Target::from_lookup(env(&[("MTTY_PANE_ID", "pane-2")])).expect("pane");
        assert_eq!(target.exe, "mtty-cli");

        // Empty values count as unset, and no pane means no reporting.
        assert_eq!(Target::from_lookup(env(&[("MTTY_PANE_ID", "")])), None);
        assert_eq!(Target::from_lookup(env(&[])), None);
    }

    #[test]
    fn arguments_name_the_pane_and_session() {
        let target = Target {
            pane: "p1".into(),
            exe: "mtty-cli".into(),
        };
        assert_eq!(
            arguments(&target, State::Processing, Some("s1")),
            vec![
                "state",
                "miao",
                "--state",
                "processing",
                "--pane",
                "p1",
                "--session",
                "s1",
                "--runtime-context",
                "-"
            ]
        );
        assert_eq!(
            arguments(&target, State::Idle, None),
            vec![
                "state",
                "miao",
                "--state",
                "idle",
                "--pane",
                "p1",
                "--runtime-context",
                "-"
            ]
        );
    }

    #[test]
    fn an_owned_runtime_context_is_reported_as_metadata() {
        let context = RuntimeContext {
            runtime_id: "runtime-1".into(),
            storage: "/tmp/miao.db".into(),
        };
        assert_eq!(
            context_json(Some(&context)),
            serde_json::json!({
                "kind": "owned",
                "runtimeID": "runtime-1",
                "storage": "/tmp/miao.db"
            })
        );
        assert_eq!(context_json(None), serde_json::Value::Null);
    }

    #[test]
    fn stopping_the_host_forces_one_final_report_without_a_context() {
        let mut reporter = Reporter {
            context: Some(RuntimeContext {
                runtime_id: "runtime-1".into(),
                storage: "/tmp/miao.db".into(),
            }),
            ..Reporter::default()
        };
        reporter.clear();
        assert_eq!(reporter.context, None);
        assert_eq!(reporter.last, Some(State::Idle));
    }

    #[test]
    fn only_changes_are_reported() {
        let mut reporter = Reporter::default();
        reporter.update(State::Processing);
        assert_eq!(reporter.last, Some(State::Processing));
        reporter.update(State::Processing);
        assert_eq!(reporter.last, Some(State::Processing));
        reporter.update(State::Idle);
        assert_eq!(reporter.last, Some(State::Idle));
        reporter.clear();
        assert_eq!(reporter.last, Some(State::Idle));
    }

    #[test]
    fn host_footer_takes_the_row_only_with_a_pane_to_report_to() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        // The capability without a pane env hides the strip with nobody
        // reporting: not a merge, just missing facts.
        assert!(!active_from_lookup(env(&[("MTTY_HOST_FOOTER", "1")])));
        assert!(active_from_lookup(env(&[
            ("MTTY_HOST_FOOTER", "1"),
            ("MTTY_PANE_ID", "p1"),
        ])));
        // A pane without the capability is the ordinary strip.
        assert!(!active_from_lookup(env(&[("MTTY_PANE_ID", "p1")])));
    }

    #[test]
    fn the_cached_footer_rides_every_report() {
        let mut reporter = Reporter::default();
        assert_eq!(reporter.footer_rider(), None);
        reporter.host_footer = true;
        reporter.last_footer = Some(r#"{"v":1,"visible":true}"#.into());
        assert_eq!(
            reporter.footer_rider().as_deref(),
            Some(r#"{"v":1,"visible":true}"#)
        );
        // Outside the merge the state path never carries a footer.
        reporter.host_footer = false;
        assert_eq!(reporter.footer_rider(), None);
    }

    #[test]
    fn host_footer_flag_reads_both_prefixes_and_ignores_empty() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        assert!(flag_from_lookup(env(&[("MTTY_HOST_FOOTER", "1")])));
        assert!(flag_from_lookup(env(&[("MIAOTTY_HOST_FOOTER", "1")])));
        assert!(!flag_from_lookup(env(&[("MTTY_HOST_FOOTER", "")])));
        assert!(!flag_from_lookup(env(&[])));
    }

    #[test]
    fn footer_wire_reports_once_and_hides_once() {
        use crate::pane::FooterWireSeg;
        let segs = vec![FooterWireSeg {
            id: "model",
            text: "deepseek-v4".into(),
            row: 23,
            start: 12,
            end: 23,
        }];
        // First sight sends; the same strip stays silent.
        let first = footer_update(&None, Some(&segs)).expect("first report");
        assert_eq!(footer_update(&Some(first.clone()), Some(&segs)), None);
        // The strip vanishing sends one hidden marker, then stays quiet.
        let hidden = footer_update(&Some(first.clone()), None).expect("hide report");
        assert!(hidden.contains(r#""visible":false"#), "{hidden}");
        assert_eq!(footer_update(&Some(hidden), None), None);
    }

    #[test]
    fn footer_json_names_the_version_and_every_segment() {
        use crate::pane::FooterWireSeg;
        let segs = vec![
            FooterWireSeg {
                id: "approval",
                text: "! auto".into(),
                row: 23,
                start: 2,
                end: 9,
            },
            FooterWireSeg {
                id: "model",
                text: "deepseek-v4".into(),
                row: 23,
                start: 12,
                end: 23,
            },
        ];
        let json = footer_json(true, &segs);
        assert!(json.contains(r#""v":1"#), "{json}");
        assert!(json.contains(crate::VERSION), "{json}");
        assert!(json.contains(r#""id":"approval""#), "{json}");
        assert!(json.contains(r#""start":2"#), "{json}");
    }
}
