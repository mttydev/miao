//! Automatic context-management trigger decisions (roadmap §2.13 gate ①).
//!
//! The anchor is the engine's normalized per-turn usage: the last `usage`
//! event's `inputTokens` (inclusive of cache read/write) plus `outputTokens`
//! approximates what the next provider turn re-sends. A provider count is an
//! input to the decision, never a precondition for it: the trigger stays armed
//! on a window the host declares, and the messages a turn has not counted yet
//! are estimated. Gate ②'s provider counting will replace the char estimate
//! for messages that have not been counted yet.

use crate::protocol::{Error, Message};
use crate::store::Store;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

/// One gate decision that kept this checkpoint idle. It rides the run's progress
/// channel for the host and, when `logged`, the engine's own log: "why did this
/// Session grow without ever compacting" used to be answerable only by reading
/// the gate code, because every rejection left the session untouched and silent.
fn idle_gate<P: FnMut(Value)>(notify: &mut P, notice: Value, logged: bool) {
    if logged {
        let gate = notice["gate"].as_u64().unwrap_or_default();
        let reason = notice["reason"].as_str().unwrap_or_default().to_owned();
        let session = notice["session_id"].as_str().unwrap_or_default().to_owned();
        crate::diagnostics::warn(
            &format!("context gate {gate} kept the checkpoint idle: {reason}"),
            &[
                ("session", session),
                ("reason", reason),
                ("detail", notice.to_string()),
            ],
        );
    }
    notify(notice);
}

/// Explicit trigger configuration from the product layer.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Effective context window for the provider. Unknown leaves the explicit
    /// headroom as the whole threshold.
    pub window: Option<u64>,
    /// Fraction of the window at which compaction triggers.
    pub ratio: f64,
    /// Explicit token headroom: trim protection + summary output + slack.
    pub headroom: u64,
}

impl Budget {
    /// The later of the explicit headroom and the configured window ratio. An
    /// unknown window degrades to the explicit headroom alone.
    pub fn threshold(&self) -> u64 {
        // A window this process never resolved stays a gap the runtime closes
        // before it asks: it names the model that will answer and reads the
        // host's window for it. Inventing a default window here would make the
        // trigger disagree with the request bound, which only ever uses the
        // window the endpoint or the host actually stated.
        match self.window {
            Some(window) if window > 0 => {
                // The product's explicit headroom is sized for a large window
                // (trim protection, summary output and slack). Taken whole it can
                // exceed a small window, and a threshold past the window is never
                // crossed: a 32K model would run to its wall with automatic
                // compaction armed but unreachable. A reserve never takes more
                // than half the window, so the trigger stays reachable — and a
                // window that small still keeps room for the summary it writes.
                let reserve = self.headroom.min(window / 2);
                // A ratio at or below one is the budget's own contract; a
                // larger one would place the trigger past the ceiling and leave
                // automatic compaction armed but unreachable. Clamp instead of
                // trusting the configuration.
                let ratio = self.ratio.clamp(0.0, 1.0);
                ((window as f64 * ratio) as u64).max(reserve).min(window)
            }
            Some(_) => 0,
            None => self.headroom,
        }
    }

    /// Whether the anchored context plus not-yet-counted messages crosses the
    /// threshold.
    pub fn should_compact(&self, context_tokens: Option<u64>, pending_tokens: u64) -> bool {
        // A missing provider count is zero known tokens, not a reason to keep
        // the history whole: requiring the measurement is what turned a missing
        // anchor into a permanent absence of compaction, so the Session grew to
        // the hard limit with nothing scheduled to shrink it.
        let threshold = self.threshold();
        // A zero threshold states no budget. Treating it as crossed would
        // compact on every settled turn when a route declared `context: 0`.
        if threshold == 0 {
            return false;
        }
        let known = context_tokens.unwrap_or(0);
        known.saturating_add(pending_tokens) >= threshold
    }
}

/// How the context window stands, in the terms the compaction trigger plans
/// against.
///
/// A pause that spends a billing allowance prints this beside that tally: the
/// allowance is the sum of what the provider charged on every turn of a prompt,
/// while occupancy is what the next request would carry, so a figure that grows
/// with the length of a prompt must never be read as a nearly full window
/// (#2870). An unknown occupancy or threshold is said out loud instead of being
/// left out, because silence reads as "the window is not the reason" only if the
/// reader already knows that no copy of the note can confirm it.
pub fn window_note(occupancy: Option<(u64, u64)>, threshold: Option<u64>) -> String {
    let Some((window, counted)) = occupancy else {
        return "the engine has no measured occupancy for this window".into();
    };
    let percent = counted.saturating_mul(100) / window.max(1);
    match threshold {
        Some(threshold) => format!(
            "the context window carries {counted} of {window} tokens ({percent}%), against a compaction threshold of {threshold} tokens"
        ),
        None => format!("the context window carries {counted} of {window} tokens ({percent}%)"),
    }
}

/// Whether a model reply claims the context window is exhausted.
///
/// A model sometimes ends an autonomous stretch by announcing the window is
/// full and asking the operator to start a new session, while the engine's
/// own occupancy says the window has room and automatic compaction is armed.
/// The claim is text, not state. The match stays conservative on purpose: it
/// fires only when a window word appears alongside an exhaustion word or an
/// explicit hand-off to a new session, so an answer that merely mentions the
/// window, or reports the compaction threshold, does not fire it.
pub fn claims_context_exhaustion(text: &str) -> bool {
    let lower = text.to_lowercase();
    let names_the_window = ["上下文", "窗口", "压缩", "context", "window", "compaction"]
        .into_iter()
        .any(|word| lower.contains(word));
    if !names_the_window {
        return false;
    }
    let claims_exhausted = [
        "用完",
        "用尽",
        "耗尽",
        "用光",
        "满了",
        "已满",
        "不够",
        "不足",
        "到头",
        "到限",
        "极限",
        "装不下",
        "exhaust",
        "used up",
        "no room",
        "ran out",
        "out of room",
        "out of space",
        "at its limit",
        "is full",
        "no longer fit",
        "cannot fit",
        "can't fit",
    ]
    .into_iter()
    .any(|word| lower.contains(word));
    let hands_off_to_a_new_session = [
        "新会话",
        "新对话",
        "新窗口",
        "new session",
        "new conversation",
        "new chat",
    ]
    .into_iter()
    .any(|word| lower.contains(word));
    claims_exhausted || hands_off_to_a_new_session
}

/// The continuation a drain injects when the model's reply claims the window
/// is exhausted while the engine's own occupancy says otherwise. The engine
/// owns the measurement, so the correction leads with the measured fact and
/// then what the loop expects: work continues here, compaction is automatic,
/// and a new session is never the way out of a window that still has room.
pub fn context_claim_correction(occupancy: Option<(u64, u64)>, threshold: Option<u64>) -> String {
    format!(
        "Your previous reply told the operator the context window is exhausted. The engine's own measurement contradicts that: {}. Automatic compaction is armed and runs before a turn that would cross the threshold, and the engine never requires a new session. Do not ask the operator to start a new session and do not restate finished work. Continue the next dependency-ready todo now with tools; if a task is genuinely blocked by an external dependency, mark that todo blocked and name exactly what is missing.",
        window_note(occupancy, threshold)
    )
}

/// The pause printed when the claim repeats past the correction bound. It
/// carries the same measured figures, so the operator can see the claim is
/// the model's rather than engine state.
pub fn context_claim_pause(occupancy: Option<(u64, u64)>, threshold: Option<u64>) -> String {
    format!(
        "Task paused: the model stopped again claiming the context window is exhausted, after corrections carrying the engine's own measurement: {}. The claim is the model's, not engine state; automatic compaction is armed and the engine never requires a new session. Nothing is running now; your next message resumes the task, and /compact forces a compaction pass immediately.",
        window_note(occupancy, threshold)
    )
}

/// Conservative char-based estimate for messages a provider has not counted
/// yet. Deliberately coarse (≈3 chars/token, rounded up) so the trigger errs
/// toward compacting; gate ② replaces it with the provider's own count.
pub fn estimate_tokens(chars: usize) -> u64 {
    (chars as u64).div_ceil(3)
}

/// Runtime wiring for automatic compaction (gates 1-6). Call at a run-owned
/// checkpoint boundary: a settled provider turn with no unsettled tools.
/// Summarization is injected so tests can stub it and the runtime can bind
/// the session's provider later. Every rejection feeds the breaker (gate 5);
/// a tripped breaker or a below-threshold context (gate 1) leaves the session
/// untouched. Gate 6 progress flows on the run's progress channel between the
/// transactional pre_compact and post_compact events the store already emits.
/// Returns whether a checkpoint was committed.
/// One run's compaction knobs at the checkpoint boundary.
/// Effective context limits from the product configuration. `None` keeps
/// automatic compaction off entirely.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextLimits {
    /// Effective context window for the provider.
    pub window: u64,
    /// Fraction of the window at which compaction triggers.
    pub ratio: f64,
    /// Explicit token headroom: trim protection + summary output + slack.
    pub headroom: u64,
    /// Summarizer passes carry at most this many selected messages. `0` leaves
    /// the size budget below to decide.
    pub per_pass: usize,
    /// A low watermark, as a fraction of the window, that starts a pass before
    /// the threshold is reached. Zero keeps the threshold as the only trigger.
    #[serde(default)]
    pub fold_ratio: f64,
    /// Serialized chars one tool-result payload may keep in the outgoing request
    /// before it is elided. Absent leaves tool output whole (pruning off).
    #[serde(default)]
    pub prune_chars: Option<usize>,
    /// Serialized chars the elidable tool results may keep in total. Absent
    /// leaves selection per-payload, which elides every candidate.
    #[serde(default)]
    pub prune_total_chars: Option<usize>,
    /// Newest user-role messages whose tool output is never elided.
    #[serde(default = "default_protect_turns")]
    pub protect_turns: usize,
    /// Which tokens count toward the threshold once a checkpoint stands.
    #[serde(default)]
    pub prefix_scope: PrefixScope,
    /// What every automatic pass must keep, in the operator's own words.
    /// Carried into the summarizer prompt as a retention target; the durable
    /// state block is retained regardless.
    #[serde(default)]
    pub retention: Option<String>,
}

fn default_protect_turns() -> usize {
    2
}

/// Which tokens a checkpoint's carried summary accounts for when the next
/// threshold is measured.
///
/// A checkpoint replaces the summarized history with one `<history-summary>`
/// message. Counting that message toward the next threshold makes a pass pay for
/// its own output: the Session compacts, then starts the next stretch already
/// partway to the trigger, and a verbose summary compacts again sooner than the
/// history actually grew. `BodyAfterPrefix` discounts the carried prefix so the
/// threshold measures growth since the checkpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrefixScope {
    /// The whole request the provider counted, carried summary included.
    #[default]
    Total,
    /// Only the history appended after the carried summary.
    #[serde(alias = "body_after_prefix")]
    BodyAfterPrefix,
}

impl ContextLimits {
    /// Limits for an operator-requested compaction on a runtime that has no
    /// automatic context management configured. There is no window to fit, so the
    /// summary is built from the selected history alone instead of retaining the
    /// exact last provider request, and elision stays off because that knob
    /// belongs to the automatic feature rather than to the operator's request.
    pub fn operator_requested() -> Self {
        Self {
            window: 0,
            ratio: 0.0,
            headroom: 0,
            fold_ratio: 0.0,
            per_pass: 0,
            prune_chars: None,
            prune_total_chars: None,
            protect_turns: default_protect_turns(),
            prefix_scope: PrefixScope::Total,
            retention: None,
        }
    }

    /// Parse the wire form the product spawner passes as
    /// `--context-limits <json>` (camelCase fields).
    pub fn from_wire(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| format!("invalid --context-limits: {error}"))
    }

    pub fn budget(&self) -> Budget {
        Budget {
            window: Some(self.window),
            ratio: self.ratio,
            headroom: self.headroom,
        }
    }

    /// The low watermark that starts a pass before the threshold, in tokens, or
    /// `None` when folding is off. A watermark past the window would never be
    /// reached, so a ratio above one is clamped here too.
    pub fn fold_threshold(&self) -> Option<u64> {
        if self.fold_ratio <= 0.0 || self.window == 0 {
            return None;
        }
        let ratio = self.fold_ratio.clamp(0.0, 1.0);
        Some(((self.window as f64 * ratio) as u64).min(self.window))
    }
}

/// One run's compaction knobs at the checkpoint boundary.
pub struct CompactRequest<'a, P: FnMut(Value)> {
    pub session: &'a str,
    pub run: &'a str,
    /// The settled assistant boundary the checkpoint cuts through.
    pub boundary: u64,
    /// An operator-selected cut. Messages after it stay verbatim, so this is
    /// also the retention target; the checkpoint commits at this sequence
    /// instead of at the settled boundary. `None` keeps the boundary.
    pub through: Option<u64>,
    /// What the summary must keep, in the operator's own words. Carried into
    /// the summarizer prompt; the durable state block is retained regardless.
    pub focus: Option<String>,
    /// What each pass of this run must keep when the request names no `focus` of
    /// its own. A configured retention target applies to automatic passes too, so
    /// an operator who always needs the same facts kept does not repeat them at
    /// every `/compact`.
    pub retention: Option<&'a str>,
    /// Which tokens the threshold compares against once a checkpoint stands.
    pub prefix_scope: PrefixScope,
    /// A low watermark that starts a pass before the window is nearly full.
    /// Folding the oldest part early keeps every pass small and keeps the
    /// Session away from the wall instead of compacting at it. `None` leaves the
    /// threshold as the only trigger, which is the default: folding more often
    /// is a quality trade, so it stays the operator's choice.
    pub fold_at: Option<u64>,
    /// An operator request: one pass happens at this boundary even when the
    /// budget gate is under its threshold. Every other gate still applies, so a
    /// forced pass cannot cut a tool continuation or a reverted transcript.
    pub forced: bool,
    pub budget: &'a Budget,
    /// Summarizer passes carry at most this many selected messages. `0` leaves
    /// the size budget below to decide.
    pub per_pass: usize,
    /// Serialized characters one summarizer call may carry. The runtime derives
    /// it from the window the pass plans against, so a long history plans calls
    /// by size rather than by message count. `0` keeps the count-only plan.
    pub segment_chars: usize,
    /// The run's own cancellation. One pass is one summarizer call per
    /// `per_pass` messages, so a long Session plans hundreds of sequential
    /// calls: an interrupt has to end the pass at the next segment instead of
    /// after the last one. A cancelled pass is the operator's decision rather
    /// than a summarizer failure, so it never feeds gate ⑤.
    pub cancel: tokio_util::sync::CancellationToken,
    pub breaker: &'a mut Breaker,
    /// Gate-6 notices flow through this sink.
    pub notify: &'a mut P,
    /// Counts the messages appended since the anchor through the provider's own
    /// endpoint where it has one (gate ②); the runtime binds the session
    /// provider. A refused or unsupported count is not an error.
    pub count: &'a mut (dyn FnMut(
        Vec<Message>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::provider::CountTokens> + Send>,
    > + Send),
}

/// How long one summarizer pass may take before the gate gives up on it.
/// A provider that accepts the request and never answers must not hold a turn.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one summarizer call gets before the pass retries it, and how long
/// that retry gets. A test drives these instead of waiting twenty seconds to
/// reach the retry path.
#[derive(Debug, Clone, Copy)]
struct Deadlines {
    first: Duration,
    retry: Duration,
}

/// The longest a summarizer call may take, however much text it carries: a pass
/// that cannot answer within five minutes is a stalled endpoint, not a slow one.
const SUMMARY_CEILING: Duration = Duration::from_secs(300);

/// Roughly how many characters one second of summarizer reading covers. The
/// deadline is derived from this rather than from a flat timeout, because the
/// history that needs compacting is exactly the history one call cannot read in
/// twenty seconds.
const SUMMARY_CHARS_PER_SECOND: usize = 5_000;

/// The budget one summarizer call gets, from the text it has to read.
///
/// The flat [`SUMMARY_TIMEOUT`] fits a small slice and is impossible for the
/// history that actually needs compacting: a pass over a few hundred thousand
/// tokens spends minutes producing its answer, so the flat deadline reported
/// every real pass as `Segment::Slow`, fed gate ⑤ through `reject`, and left the
/// Session told that automatic compaction was paused — in the one situation the
/// shrink exists for. The budget now grows with the slice, between
/// [`SUMMARY_TIMEOUT`] and [`SUMMARY_CEILING`]; a retry re-reads the same text
/// and earns the same budget, because what a second call rescues is a stalled
/// transport rather than a slow model.
fn deadlines_for(chars: usize) -> Deadlines {
    let first = (SUMMARY_TIMEOUT
        + Duration::from_secs((chars / SUMMARY_CHARS_PER_SECOND.max(1)) as u64))
    .min(SUMMARY_CEILING);
    Deadlines {
        first,
        retry: first,
    }
}

/// Reject one pass and keep the reason. The progress notice is ephemeral by
/// design, so a Session that keeps failing to compact used to be diagnosable
/// only from an operator's memory of a toast; the store keeps the reason.
async fn reject(
    store: &Store,
    session: &str,
    run: &str,
    reason: &str,
    context: u64,
    breaker: &mut Breaker,
) {
    breaker.rejected(context);
    let _ = store
        .record(
            session,
            "compaction.rejected",
            json!({"run_id": run, "reason": reason}),
        )
        .await;
}

pub async fn maybe_compact<S, F, P>(
    store: &Store,
    request: CompactRequest<'_, P>,
    summarize: S,
) -> Result<bool, Error>
where
    S: FnMut(String) -> F,
    F: Future<Output = Result<String, String>>,
    P: FnMut(Value),
{
    maybe_compact_with_prefix(store, request, summarize, || async { None }).await
}

/// The optional prefix call retains the exact last provider request and only
/// appends settled history plus a retention instruction. Unsupported adapters,
/// oversized contexts and failed prefix attempts retain the bounded cold path.
pub async fn maybe_compact_with_prefix<S, F, P, H, HF>(
    store: &Store,
    request: CompactRequest<'_, P>,
    mut summarize: S,
    mut prefix: H,
) -> Result<bool, Error>
where
    S: FnMut(String) -> F,
    F: Future<Output = Result<String, String>>,
    H: FnMut() -> HF,
    HF: Future<Output = Option<Result<String, String>>>,
    P: FnMut(Value),
{
    let CompactRequest {
        session,
        run,
        boundary,
        through,
        focus,
        retention,
        prefix_scope,
        forced,
        fold_at,
        budget,
        per_pass,
        segment_chars,
        cancel,
        breaker,
        notify,
        count,
    } = request;
    // A directed cut replaces the settled boundary: the checkpoint commits where
    // the operator asked, and everything newer stays in the prefix verbatim.
    let boundary = through.unwrap_or(boundary);
    let channel = |kind: &str| json!({"session_id": session, "run_id": run, "kind": kind});
    // A pass belongs to the turn that asked for it. An interrupt already said
    // this turn is over, so it must not start spending summarizer calls that
    // only the end of the turn would abandon.
    if cancel.is_cancelled() {
        return Ok(false);
    }
    // Gate ①: the anchor is the newest usage report's normalized context plus
    // the message it counted. Only the messages appended after that report are
    // still uncounted, and for those gate ② prefers the provider's own count
    // over the char estimate. No marker means the report predates the anchor,
    // so nothing is invented and the trigger stays off. An operator request
    // needs neither: it asks for the selected history to be summarized now.
    let (context, pending_tokens) = match store.context_anchor(session).await? {
        Some((context, counted_through)) => {
            let (pending, pending_chars) = store.pending_context(session, counted_through).await?;
            if pending.is_empty() && !forced {
                idle_gate(
                    notify,
                    json!({
                        "session_id": session, "run_id": run,
                        "kind": "compaction.skipped", "gate": 1,
                        "reason": "nothing_pending", "context_tokens": context,
                    }),
                    true,
                );
                return Ok(false);
            }
            (
                context,
                resolve_pending_tokens(count(pending).await, pending_chars as usize),
            )
        }
        None if forced => (0, 0),
        // A Session with no marked usage report yet: the whole selected history
        // is uncounted, which is a measurement this trigger may estimate, not a
        // reason to leave the history whole until the endpoint refuses it. Gate
        // ② still prefers the provider's count over the char estimate.
        None => {
            let (pending, pending_chars) = store.pending_context(session, 0).await?;
            idle_gate(
                notify,
                json!({
                    "session_id": session, "run_id": run,
                    "kind": "compaction.skipped", "gate": 1,
                    "reason": "no_context_anchor",
                    "messages": pending.len(), "chars": pending_chars,
                }),
                true,
            );
            (
                0,
                resolve_pending_tokens(count(pending).await, pending_chars as usize),
            )
        }
    };
    let total = context.saturating_add(pending_tokens);
    // A carried summary stands in for the history it replaced, so counting it
    // again charges the next pass for the previous one's own output. The scoped
    // measurement discounts it and leaves the threshold reading growth since the
    // checkpoint; the default keeps counting the request the provider counted.
    let carried = match prefix_scope {
        PrefixScope::Total => 0,
        PrefixScope::BodyAfterPrefix => store.carried_prefix_tokens(session).await?,
    };
    let measured = match prefix_scope {
        PrefixScope::Total => context,
        PrefixScope::BodyAfterPrefix => context.saturating_sub(carried),
    };
    // Two triggers: the threshold that says the next request is about to be
    // refused, and the optional low watermark that keeps passes small instead of
    // making every pass pay for the whole history.
    let folding = fold_at.is_some_and(|fold| total >= fold);
    if !forced && !folding && !budget.should_compact(Some(measured), pending_tokens) {
        // Below half the threshold nothing is near the wall, and one line per
        // settled turn would bury the case somebody is investigating; the
        // progress notice still goes out every time.
        idle_gate(
            notify,
            json!({
                "session_id": session, "run_id": run,
                "kind": "compaction.skipped", "gate": 1,
                "reason": "under_threshold",
                "context_tokens": context, "pending_tokens": pending_tokens,
                "counted_tokens": measured,
                "carried_prefix_tokens": carried,
                "window": budget.window,
                "threshold": budget.threshold(),
            }),
            total.saturating_mul(2) >= budget.threshold(),
        );
        return Ok(false);
    }
    // Gate ⑤ runs after the size is known: a tripped breaker may open a
    // half-open attempt once the history has grown past the size that failed.
    // Checking it before the anchor made the trip permanent for the rest of the
    // Session, so a summarizer that failed once could never compact again and the
    // history grew until the endpoint refused every request.
    if !breaker.allows_at(total) {
        idle_gate(
            notify,
            json!({
                "session_id": session, "run_id": run,
                "kind": "compaction.skipped", "gate": 5, "reason": "breaker_open",
                "context_tokens": context, "pending_tokens": pending_tokens,
            }),
            true,
        );
        // The trip itself is durable through the `compaction.rejected` streak,
        // but the stretches this Session spent unable to try were log-only: a
        // per-workspace store that outlives the channel's log keeps the answer
        // to "why did this Session grow without compacting".
        let _ = store
            .record(
                session,
                "compaction.skipped",
                json!({
                    "run_id": run, "gate": 5, "reason": "breaker_open",
                    "context_tokens": context, "pending_tokens": pending_tokens,
                }),
            )
            .await;
        return Ok(false);
    }
    // The summary covers the warm prefix the next turn would re-send, which is
    // the selected history rather than the uncounted delta. A directed cut
    // summarizes exactly the range it names.
    let messages = match through {
        Some(cut) => store.selected_input_through(session, cut).await?.0,
        None => store.selected_history(session).await?,
    };
    if messages.is_empty() {
        idle_gate(
            notify,
            json!({
                "session_id": session, "run_id": run,
                "kind": "compaction.skipped", "gate": 1,
                "reason": "no_selected_history",
            }),
            true,
        );
        return Ok(false);
    }
    // The budget is what keeps a long history to a handful of calls; the message
    // cap only bounds one answer's size, so it is applied when the caller set it.
    let sizes: Vec<usize> = messages
        .iter()
        .map(|message| message_excerpt(message, SUMMARIZER_MESSAGE_CHARS).len() + 1)
        .collect();
    // What the prefix attempt would have to read, so its deadline is derived
    // from the same text the segmented path is.
    let history_chars = sizes.iter().sum::<usize>();
    let plan = if segment_chars > 0 {
        plan_summary_by_size(&sizes, per_pass, segment_chars)
    } else {
        plan_summary(messages.len(), per_pass)
    };
    notify(json!({
        "session_id": session, "run_id": run,
        "kind": "compaction.started", "segments": plan.segments.len(),
    }));
    // A pass can take minutes over a long history, and its outcome lands in the
    // store only on success (`compaction.settled`) or on a summarizer failure
    // (`compaction.rejected`); the start itself was progress-only, so a pass the
    // turn abandoned mid-flight left no trace at all.
    let _ = store
        .record(
            session,
            "compaction.started",
            json!({"run_id": run, "segments": plan.segments.len()}),
        )
        .await;
    let mut parts = Vec::new();
    // The prefix path retains the exact last provider request, which describes
    // one particular cut. A scoped request names its own range and target, so it
    // always takes the segment path.
    // The prefix path carries the request this Session actually sent, so it is
    // the most faithful summary input there is. It used to be refused whenever
    // the history was over the window — exactly when compaction is needed —
    // because the request did not fit; the closure now elides tool output to
    // fit instead of skipping the path.
    if through.is_none() && focus.is_none() {
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            outcome = tokio::time::timeout(deadlines_for(history_chars).first, prefix()) => Some(outcome),
        };
        let Some(outcome) = outcome else {
            notify(channel("compaction.cancelled"));
            return Ok(false);
        };
        match outcome {
            Ok(Some(Ok(summary))) if !summary.trim().is_empty() => {
                parts.push(summary);
                notify(
                    json!({"session_id":session,"run_id":run,"kind":"compaction.progress","segment":1,"total":1,"path":"prefix"}),
                );
            }
            Ok(None) => {}
            outcome => {
                let reason = match outcome {
                    Err(_) => "prefix summary timeout".to_owned(),
                    Ok(Some(Err(reason))) => reason,
                    _ => "prefix summary was empty".to_owned(),
                };
                notify(
                    json!({"session_id":session,"run_id":run,"kind":"compaction.prefix.fallback","reason":reason}),
                );
            }
        }
    }
    if parts.is_empty() {
        for (nth, (start, end)) in plan.segments.iter().enumerate() {
            // One pass plans one summarizer call per `per_pass` messages, so a
            // long Session is minutes of quiet work. An interrupt ends it here:
            // the operator asked to stop this turn, and stopping is not a
            // summarizer failure, so gate ⑤ must not count it.
            if cancel.is_cancelled() {
                notify(json!({
                    "session_id": session, "run_id": run,
                    "kind": "compaction.cancelled",
                    "segment": nth, "total": plan.segments.len(),
                }));
                return Ok(false);
            }
            let body = summarizer_body(&messages[*start..*end], SUMMARIZER_MESSAGE_CHARS);
            // A retention target is an operator instruction, not a replacement
            // for the durable block: the checkpoint carries both.
            let instruction = match focus.as_deref().or(retention) {
                Some(focus) => format!(
                    "Summarize this portion of the conversation:\n{body}\n\nRetention target: {focus}"
                ),
                None => format!("Summarize this portion of the conversation:\n{body}"),
            };
            // A summarizer that never returns must not hold the turn: the call is
            // bounded, and a timeout degrades like any other summary failure.
            let deadline = deadlines_for(body.len());
            match summarize_segment(&mut summarize, &instruction, &cancel, deadline).await {
                Segment::Cancelled => {
                    notify(json!({
                        "session_id": session, "run_id": run,
                        "kind": "compaction.cancelled",
                        "segment": nth, "total": plan.segments.len(),
                    }));
                    return Ok(false);
                }
                Segment::Slow => {
                    // A pass that summarized segments before this one failed has
                    // done real work. Counting it against gate ⑤ would disable
                    // automatic compaction for a Session that is merely slow at
                    // the end, which is how a long history stopped compacting
                    // itself after three unlucky segments.
                    let partial = !parts.is_empty();
                    if !partial {
                        reject(
                            store,
                            session,
                            run,
                            "summary timeout",
                            context.saturating_add(pending_tokens),
                            breaker,
                        )
                        .await;
                    }
                    notify(json!({
                        "session_id": session, "run_id": run,
                        "kind": "compaction.failed", "reason": "summary timeout",
                        "partial": partial,
                    }));
                    return Ok(false);
                }
                Segment::Answered(Ok(part)) => {
                    parts.push(part);
                    notify(json!({
                        "session_id": session, "run_id": run,
                        "kind": "compaction.progress", "segment": nth + 1, "total": plan.segments.len(),
                    }));
                }
                Segment::Answered(Err(reason)) => {
                    // The summarizer must send the very history that overflowed
                    // the Session, so it can overflow too — and then the Session
                    // stays stuck retrying the same doomed slice. Retry once
                    // against a hard-truncated body: newest text that fits the
                    // window, no second model round over the full slice. Only an
                    // overflow-shaped refusal earns the retry; any other failure
                    // still answers the request on the first attempt.
                    let retry = budget
                        .window
                        .filter(|_| looks_like_overflow(&reason))
                        .and_then(|window| {
                            let room = window.saturating_sub(budget.headroom.max(8_192));
                            let trimmed = truncated_body(&body, (room.saturating_mul(3)) as usize);
                            (trimmed.len() < body.len()).then_some(trimmed)
                        });
                    let Some(trimmed) = retry else {
                        // Same rule as a slow segment: work already summarized in
                        // this pass is not a reason to disable the trigger.
                        let partial = !parts.is_empty();
                        if !partial {
                            reject(
                                store,
                                session,
                                run,
                                &reason,
                                context.saturating_add(pending_tokens),
                                breaker,
                            )
                            .await;
                        }
                        notify(json!({
                            "session_id": session, "run_id": run,
                            "kind": "compaction.failed", "reason": reason, "partial": partial,
                        }));
                        return Ok(false);
                    };
                    let instruction = format!(
                        "Summarize this portion of the conversation; older history was truncated to fit the model window, so keep facts, decisions and the durable state block:\n{trimmed}"
                    );
                    match summarize_segment(
                        &mut summarize,
                        &instruction,
                        &cancel,
                        deadlines_for(instruction.len()),
                    )
                    .await
                    {
                        Segment::Answered(Ok(part)) => {
                            parts.push(part);
                            notify(json!({
                                "session_id": session, "run_id": run,
                                "kind": "compaction.progress", "segment": nth + 1,
                                "total": plan.segments.len(), "truncated": true,
                            }));
                        }
                        Segment::Cancelled => {
                            notify(json!({
                                "session_id": session, "run_id": run,
                                "kind": "compaction.cancelled",
                                "segment": nth, "total": plan.segments.len(),
                            }));
                            return Ok(false);
                        }
                        _ => {
                            let partial = !parts.is_empty();
                            if !partial {
                                reject(
                                    store,
                                    session,
                                    run,
                                    &reason,
                                    context.saturating_add(pending_tokens),
                                    breaker,
                                )
                                .await;
                            }
                            notify(json!({
                                "session_id": session, "run_id": run,
                                "kind": "compaction.failed", "reason": reason,
                                "truncated": true, "partial": partial,
                            }));
                            return Ok(false);
                        }
                    }
                }
            }
        }
    }
    let id = format!("cut_{run}_{boundary}");
    match store
        .compact_for_run(
            session,
            run,
            &id,
            boundary,
            checkpoint_summary(store, &parts, session).await,
        )
        .await
    {
        Ok(_) => {
            notify(channel("compaction.finished"));
            Ok(true)
        }
        Err(error) => {
            let reason = error.to_string();
            reject(
                store,
                session,
                run,
                &reason,
                context.saturating_add(pending_tokens),
                breaker,
            )
            .await;
            notify(json!({
                "session_id": session, "run_id": run,
                "kind": "compaction.failed", "reason": reason,
            }));
            Ok(false)
        }
    }
}

/// How one segment's summarizer call ended.
enum Segment {
    /// The summarizer answered: its text, or why the text was unusable.
    Answered(Result<String, String>),
    /// The call ran out of both deadlines.
    Slow,
    /// The pass was interrupted while the call was in flight.
    Cancelled,
}

/// Summarize one segment under the pass deadlines, retrying a missed deadline
/// once. Reaching the caller as a failure used to mean the whole pass — every
/// segment summarized before it — was thrown away and counted against gate ⑤.
async fn summarize_segment<S, F>(
    summarize: &mut S,
    instruction: &str,
    cancel: &tokio_util::sync::CancellationToken,
    deadlines: Deadlines,
) -> Segment
where
    S: FnMut(String) -> F,
    F: Future<Output = Result<String, String>>,
{
    if let Ok(answered) =
        tokio::time::timeout(deadlines.first, summarize(instruction.to_owned())).await
    {
        return settled(answered, cancel);
    }
    if cancel.is_cancelled() {
        return Segment::Cancelled;
    }
    match tokio::time::timeout(deadlines.retry, summarize(instruction.to_owned())).await {
        Ok(answered) => settled(answered, cancel),
        Err(_) if cancel.is_cancelled() => Segment::Cancelled,
        Err(_) => Segment::Slow,
    }
}

/// A summarizer call the interrupt aborted reports a transport error, and that
/// is the stop the operator asked for rather than a summarizer failure: only a
/// call that failed on its own may feed gate ⑤.
fn settled(
    answered: Result<String, String>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Segment {
    if answered.is_err() && cancel.is_cancelled() {
        return Segment::Cancelled;
    }
    Segment::Answered(answered)
}

/// Elide tool output until a summarizer request fits `window` tokens, largest
/// payload first, naming each elided payload by the handle `recall` restores.
///
/// The exact last request is the most faithful summary input a Session can give
/// the summarizer, so trimming it is better than skipping that path: skipping is
/// what made a Session at the window's edge fall back to summarizing its whole
/// history one small slice at a time. Returns whether the request now fits; a
/// request whose text alone cannot be trimmed enough tells the caller to take
/// the segmented path, which never sends the whole history in one call.
pub fn fit_request(messages: &mut [Message], window: u64, headroom: u64) -> bool {
    // `estimate_tokens` charges three characters per token, so the character
    // budget is three times the tokens the window leaves.
    let budget = (window.saturating_sub(headroom) as usize).saturating_mul(3);
    let mut payloads = Vec::new();
    let mut total = 0usize;
    for (message, body) in messages.iter().enumerate() {
        let Some(blocks) = body.content.as_array() else {
            continue;
        };
        for (block, part) in blocks.iter().enumerate() {
            // Prose counts toward the window exactly like tool output does; only
            // tool output can be elided, because only it has a handle to restore.
            let text = part
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let prose = part.get("text").and_then(Value::as_str).unwrap_or_default();
            total = total.saturating_add(text.len()).saturating_add(prose.len());
            if part["type"] != "tool_result" || text.is_empty() {
                continue;
            }
            // The handle is built while the message is only borrowed: naming it
            // after the elision would borrow the same message the loop is about
            // to mutate.
            let handle = match part["tool_use_id"].as_str() {
                Some(id) if !id.is_empty() && id.len() <= 128 => {
                    format!(
                        r#"[tool output archived — recall {{"tool_use_id":"{id}"}} to restore]"#
                    )
                }
                _ => "[oversized tool output omitted]".to_string(),
            };
            payloads.push((message, block, text.len(), handle));
        }
    }
    if total <= budget {
        return true;
    }
    payloads.sort_by_key(|(_, _, len, _)| std::cmp::Reverse(*len));
    for (message, block, len, handle) in payloads {
        if total <= budget {
            break;
        }
        let Some(part) = messages[message].content.get_mut(block) else {
            continue;
        };
        part["content"] = json!(handle);
        total = total.saturating_sub(len).saturating_add(handle.len());
    }
    total <= budget
}

/// Per-message cap on what one summarizer pass reads from a stored message.
/// Oversized tool outputs are named by their recall handle instead of being
/// paid for twice (the summary pass would re-read what pruning already
/// elided from the outgoing request).
pub const SUMMARIZER_MESSAGE_CHARS: usize = 2_000;

/// `commit_compaction` rejects checkpoints above this size, so the assembly
/// truncates the summarized part rather than failing the whole compaction.
pub const MAX_CHECKPOINT_CHARS: usize = 32_768;

/// The serialized text of one message, bounded for a summarizer pass. Text
/// blocks are capped at a char boundary; oversized tool results are replaced
/// by their recall handle, so nothing is lost that pruning has not already
/// archived.
fn message_excerpt(message: &Message, budget: usize) -> String {
    let blocks = message.content.as_array().into_iter().flatten();
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or_default();
                parts.push(bounded(text, budget));
            }
            Some("tool_result") => {
                let content = block["content"].as_str().unwrap_or_default();
                if content.len() <= budget {
                    parts.push(content.to_owned());
                } else {
                    let handle = match block["tool_use_id"].as_str() {
                        Some(id) if id.len() <= 128 && !id.is_empty() => {
                            format!(
                                r#"[tool output archived — recall {{"tool_use_id":"{id}"}} to restore]"#
                            )
                        }
                        _ => "[oversized tool output omitted]".to_owned(),
                    };
                    parts.push(handle);
                }
            }
            _ => {}
        }
    }
    format!("- {}: {}", message.role, parts.join(" "))
}

/// The largest char boundary at or below `budget`, with an explicit marker
/// when anything was cut.
pub(crate) fn bounded(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut cut = budget.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[truncated]", &text[..cut])
}

/// One summarizer pass's input: one bounded line per message.
/// Whether a summarizer refusal reads like a context overflow. The retry is
/// only honest for those: truncating the body can fix an oversized request,
/// and it cannot fix any other refusal.
fn looks_like_overflow(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("context")
        || reason.contains("too long")
        || reason.contains("exceed")
        || reason.contains("maximum")
}

/// Keep the newest `max_chars` characters of a summarizer body: the tail holds
/// the state the next turn needs most, and dropping from the front is what
/// makes the retry fit the window at all.
fn truncated_body(body: &str, max_chars: usize) -> String {
    if body.len() <= max_chars {
        return body.to_owned();
    }
    let mut cut = body.len() - max_chars;
    while cut < body.len() && !body.is_char_boundary(cut) {
        cut += 1;
    }
    format!(
        "(older conversation truncated to fit the model window)\n{}",
        &body[cut..]
    )
}

pub fn summarizer_body(messages: &[Message], per_message_chars: usize) -> String {
    messages
        .iter()
        .map(|message| message_excerpt(message, per_message_chars))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Durable state carried verbatim into every checkpoint, independent of
/// summarizer quality: the active objective and the open tasks. Completed or
/// cancelled tasks are history, not state, and stay out.
pub fn durable_state_block(state: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    let goal = &state["goal"]["value"];
    if goal["objective"].as_str().is_some_and(|o| !o.is_empty()) {
        lines.push(format!(
            "Objective: {} (status: {})",
            goal["objective"].as_str().unwrap_or_default(),
            goal["status"].as_str().unwrap_or("unknown")
        ));
    }
    let todos = state["todos"]["value"]["todos"].as_array();
    let open = todos
        .into_iter()
        .flatten()
        .filter(|todo| {
            todo["status"].as_str() != Some("completed")
                && todo["status"].as_str() != Some("cancelled")
        })
        .filter_map(|todo| todo["content"].as_str())
        .collect::<Vec<_>>();
    if !open.is_empty() {
        lines.push("Open tasks:".to_owned());
        lines.extend(open.iter().map(|task| format!("- [ ] {task}")));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("## Durable state (carried verbatim)\n{}", lines.join("\n"))
}

/// The checkpoint text: summarized parts plus the durable-state block, within
/// the size `commit_compaction` accepts. The durable block is the part that
/// must never be lost, so overflow truncates the summarized part instead.
pub async fn checkpoint_summary(store: &Store, parts: &[String], session: &str) -> String {
    let mut summary = parts.join("\n\n");
    let durable = store
        .state(session)
        .await
        .map(|state| durable_state_block(&state))
        .unwrap_or_default();
    if durable.is_empty() {
        return bounded_to(&summary, MAX_CHECKPOINT_CHARS);
    }
    let combined = format!("{summary}\n\n{durable}");
    if combined.len() <= MAX_CHECKPOINT_CHARS {
        return combined;
    }
    let keep = MAX_CHECKPOINT_CHARS.saturating_sub(durable.len() + 2);
    summary = bounded_to(&summary, keep);
    format!("{summary}\n\n{durable}")
}

/// The largest char-boundary prefix within `max` chars.
fn bounded_to(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_owned()
}

/// Gate 5: the compaction circuit breaker. After `threshold` consecutive
/// rejected attempts the breaker trips so a persistently failing summarizer
/// cannot spin the engine. It is not permanent: a Session whose history keeps
/// growing past the size that already failed gets one more attempt (half-open),
/// because a breaker that stays tripped while the context grows only guarantees
/// that the history reaches the endpoint's hard ceiling, where every request is
/// refused and the Session can no longer be continued at all.
#[derive(Debug, Clone)]
pub struct Breaker {
    threshold: u8,
    failures: u8,
    /// Context tokens that were being compacted when an attempt last failed.
    tripped_at: Option<u64>,
    /// Settled boundaries a tripped breaker has waited through. Size alone stops
    /// being a retry signal once the request is bounded deterministically: the
    /// history no longer grows past the size that failed, so a summarizer the
    /// provider repaired would never be tried again.
    waited: u8,
}

/// How many settled boundaries a tripped breaker waits before opening one more
/// attempt. Each attempt costs one summarizer call, so one per window cannot
/// spin; a run that fails again still needs the full streak to trip it.
const RETRY_AFTER_BOUNDARIES: u8 = 4;

impl Breaker {
    pub fn new(threshold: u8) -> Self {
        Self {
            threshold,
            failures: 0,
            tripped_at: None,
            waited: 0,
        }
    }

    /// A rejected compaction attempt. Reaching the threshold trips the breaker,
    /// remembering how large the history was so a later, larger one can retry.
    pub fn rejected(&mut self, context: u64) {
        self.tripped_at = Some(context);
        self.waited = 0;
        if self.failures < u8::MAX {
            self.failures = self.failures.saturating_add(1);
        }
    }

    /// An explicit operator compact clears the failure streak and closes the
    /// breaker.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.tripped_at = None;
        self.waited = 0;
    }

    /// A committed pass. The streak is consecutive, so a summarizer that
    /// answered starts clean: keeping the half-open allowance would let one
    /// later rejection trip a breaker that had just succeeded, and `idle()`
    /// could never become true again.
    pub fn committed(&mut self) {
        self.reset();
    }

    /// Whether compaction may be attempted.
    pub fn allows(&self) -> bool {
        self.failures < self.threshold
    }

    /// Whether compaction may be attempted at this context size. A tripped
    /// breaker opens one half-open attempt once the history has grown at least a
    /// tenth past the size that failed: the same request would fail again, but a
    /// materially larger one is a different attempt, and leaving it refused is
    /// how a Session reaches the endpoint's ceiling.
    ///
    /// A Session whose requests are bounded deterministically never grows that
    /// way, so the same breaker also opens one attempt after
    /// `RETRY_AFTER_BOUNDARIES` settled boundaries. Without it a repaired
    /// summarizer would stay off for the life of that Session, which is the
    /// silent half of the failure this gate is supposed to prevent.
    pub fn allows_at(&mut self, context: u64) -> bool {
        if self.allows() {
            return true;
        }
        if let Some(tripped) = self.tripped_at {
            if context.saturating_mul(10) >= tripped.saturating_mul(11) {
                self.failures = self.threshold.saturating_sub(1);
                self.waited = 0;
                return true;
            }
        }
        self.waited = self.waited.saturating_add(1);
        if self.waited < RETRY_AFTER_BOUNDARIES {
            return false;
        }
        self.waited = 0;
        self.failures = self.threshold.saturating_sub(1);
        true
    }

    /// Nothing worth remembering: no failure streak and no tripped size, so a
    /// Session whose summarizer is answering leaves no state behind.
    pub fn idle(&self) -> bool {
        self.failures == 0 && self.tripped_at.is_none()
    }
}

/// Gate 4: one tool output's pruning decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prune {
    /// The output stays whole.
    Keep,
    /// The output is replaced by a bounded head and tail with an elision mark.
    Elide { keep_head: usize, keep_tail: usize },
}

/// Gate ④: prune oversized tool outputs from the context selection. Outputs
/// within `budget` chars stay whole; anything larger keeps a head/tail
/// reserve plus an explicit elision mark, so a model can still see the
/// shape of what it received. Recent outputs (the newest `protect` tool
/// messages, which the turn is most likely acting on) are never pruned
/// regardless of size.
pub fn prune_tool_output(output_len: usize, budget: usize, is_recent: bool) -> Prune {
    if is_recent || output_len <= budget {
        return Prune::Keep;
    }
    // Half of the budget for the head, a quarter for the tail, and the rest
    // for the elision mark and framing, bounded so tiny budgets still keep
    // something recognizable on both ends.
    let keep_head = (budget / 2).max(64);
    let keep_tail = (budget / 4).max(32);
    Prune::Elide {
        keep_head,
        keep_tail,
    }
}

/// The elision mark's text. A model that reads an opaque placeholder as a
/// low-context signal invents a stop reason (observed on the TypeScript path),
/// so the mark states the facts instead of looking like a truncation.
pub const PRUNE_MARK: &str = "\n…[older tool output elided by context management — routine pruning, not a low-context signal; the harness compacts automatically near the window limit, so keep working]…\n";

/// The payload and total character budgets used when a request no longer fits
/// the window and the operator configured none. Old tool output is the
/// cheapest thing to drop first: the durable transcript keeps every byte, and
/// a Session whose request fits is left untouched, so its cache prefix stays
/// stable. A blanket default would re-elide output on every turn and pay a
/// cache miss for savings the request did not need.
pub const DEFAULT_PRUNE_CHARS: usize = 16 * 1024;
pub const DEFAULT_PRUNE_TOTAL_CHARS: usize = 256 * 1024;

/// Gate ④: elide oversized tool results from the outgoing request only, never
/// from stored history. The newest `protect_turns` user-role messages (prompts
/// or tool results) keep their output whatever it costs, so the work in flight
/// keeps its data. Returns the elided payload count and the characters dropped.
pub fn prune_history(
    messages: &mut [Message],
    budget: usize,
    protect_turns: usize,
    total: Option<usize>,
) -> (usize, usize) {
    let stats = prune_history_with_stats(messages, budget, protect_turns, total);
    (stats.payloads, stats.before_bytes - stats.after_bytes)
}

/// Measured UTF-8 payload bytes, including the replacement marker in `after`.
/// These are request-only tool results, not tokens or the full context window.
#[derive(Default)]
pub struct PruneStats {
    pub payloads: usize,
    pub messages: usize,
    pub before_bytes: usize,
    pub after_bytes: usize,
}

/// What one hard trim dropped from the outgoing request.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TrimStats {
    pub messages: usize,
    pub before_bytes: usize,
    pub after_bytes: usize,
}

/// Whether the message carries a tool result, which is only valid directly after
/// the assistant message that called the tool.
fn carries_tool_result(message: &Message) -> bool {
    message
        .content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
}

/// The last-resort bound on an assembled request.
///
/// Compaction is the planned way to shrink a Session, but it needs a summarizer
/// the endpoint will answer. When that is unavailable — no configured window, a
/// summarizer the provider keeps refusing — the history grows past the window and
/// every request is refused, which is how a Session stops being continuable.
/// Dropping the oldest turns keeps the newest work and the operator's prompt, so
/// the next request is served instead of refused. The durable transcript is
/// untouched: this bounds one request, exactly like tool-output pruning does.
///
/// The estimate over-counts on purpose (two bytes per token). This only runs when
/// the request is already at the window, so trimming a little early is safe and
/// under-counting is not: the sessions that reach it are usually CJK-heavy, where
/// a three-bytes-per-token estimate is off by a factor of three.
pub fn trim_history_to_fit(messages: &mut Vec<Message>, budget: u64) -> TrimStats {
    let mut stats = TrimStats::default();
    let sizes = messages
        .iter()
        .map(|message| {
            serde_json::to_vec(message)
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut total = sizes.iter().sum::<usize>();
    stats.before_bytes = total;
    // Trim to below nine tenths of the budget rather than to the boundary.
    // Left at the boundary, one tool result puts the request back over the
    // limit and the engine trims again on the very next step — observed as
    // 11,306 trims across 2,115 turns. The headroom buys several steps before
    // the next trim, and the oldest history beyond it is what a bounded window
    // cannot carry anyway.
    let target = budget.saturating_sub(budget / 10);
    let over = |total: usize| (total as u64).div_ceil(2) > target;
    let mut dropped = 0;
    // The trailing message is the operator's prompt: without it the request has
    // nothing to do, so it is never dropped.
    while dropped + 1 < messages.len() && over(total) {
        let mut cut = dropped + 1;
        // A request has to start on a user turn: a leading assistant message, or a
        // tool result whose call was just dropped, is not a valid prefix. The cut
        // continues to the next clean user message instead of emitting one.
        while cut < messages.len()
            && (messages[cut].role != "user" || carries_tool_result(&messages[cut]))
        {
            cut += 1;
        }
        if cut >= messages.len() {
            break;
        }
        for size in &sizes[dropped..cut] {
            total = total.saturating_sub(*size);
        }
        dropped = cut;
    }
    if dropped > 0 {
        messages.drain(..dropped);
        stats.messages = dropped;
    }
    stats.after_bytes = total;
    stats
}

pub fn prune_history_with_stats(
    messages: &mut [Message],
    budget: usize,
    protect_turns: usize,
    total: Option<usize>,
) -> PruneStats {
    // Selection is budget-aware. The candidates are the payloads above the
    // per-payload budget outside the protected turns; with a total target
    // configured, only as many of them -- oldest first, because the work in
    // flight is least likely to still need the oldest -- are elided as make the
    // remaining candidates fit that target. Without one, every candidate is.
    let candidates = prune_candidates(messages, budget, protect_turns);
    let required = total.map_or(usize::MAX, |target| {
        candidates
            .iter()
            .map(|(_, _, len)| *len)
            .sum::<usize>()
            .saturating_sub(target)
    });
    let mut stats = PruneStats::default();
    let mut previous_message = None;
    for (index, block, len) in candidates {
        if stats.before_bytes - stats.after_bytes >= required {
            break;
        }
        let pruned = {
            let value = &messages[index].content[block];
            let Some(content) = value["content"].as_str() else {
                continue;
            };
            let Prune::Elide {
                keep_head,
                keep_tail,
            } = prune_tool_output(len, budget, false)
            else {
                continue;
            };
            let head = floor_boundary(content, keep_head);
            let tail = content.len() - ceil_boundary_from_end(content, keep_tail);
            // Pruning stays recoverable: the marker names the handle that
            // restores the elided middle, so nothing is silently lost.
            let note = match value["tool_use_id"].as_str() {
                Some(id) if id.len() <= 128 => format!(
                    "[the elided middle is recoverable on demand: recall {{\"tool_use_id\":\"{id}\"}}]\n"
                ),
                _ => String::new(),
            };
            format!(
                "{}{}{}{}",
                &content[..head],
                PRUNE_MARK,
                note,
                &content[tail..]
            )
        };
        // The marker and recall handle count too. Near-budget outputs and
        // tiny budgets may otherwise grow the request and underflow savings.
        if pruned.len() >= len {
            continue;
        }
        stats.before_bytes += len;
        stats.after_bytes += pruned.len();
        stats.messages += usize::from(previous_message != Some(index));
        previous_message = Some(index);
        messages[index].content[block]["content"] = json!(pruned);
        stats.payloads += 1;
    }
    stats
}

/// The payloads elision may consider: above the per-payload budget and outside
/// the newest `protect_turns` user-role messages, oldest first, each with its
/// message index, content-block index, and byte length.
fn prune_candidates(
    messages: &[Message],
    budget: usize,
    protect_turns: usize,
) -> Vec<(usize, usize, usize)> {
    let turns_total = messages
        .iter()
        .filter(|message| message.role == "user")
        .count();
    let protected_from = turns_total.saturating_sub(protect_turns);
    let mut turns = 0;
    let mut candidates = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role == "user" {
            turns += 1;
        }
        if turns > protected_from {
            continue;
        }
        for (block, part) in message.content.as_array().into_iter().flatten().enumerate() {
            if part["type"] != "tool_result" {
                continue;
            }
            let Some(content) = part["content"].as_str() else {
                continue;
            };
            if !matches!(
                prune_tool_output(content.len(), budget, false),
                Prune::Elide { .. }
            ) {
                continue;
            }
            candidates.push((index, block, content.len()));
        }
    }
    candidates
}

/// The largest char boundary at or below `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut cut = index.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// The smallest char boundary at or above `len - index`.
fn ceil_boundary_from_end(text: &str, index: usize) -> usize {
    let mut cut = index.min(text.len());
    while !text.is_char_boundary(text.len() - cut) {
        cut -= 1;
    }
    cut
}

/// Gate ③: how a hot prefix is summarized. One pass while the prefix fits
/// the summarizer's budget; consecutive segments only when it cannot. Ranges
/// are half-open `[start, end)` message offsets, so a caller replays them
/// directly against the selected history and the checkpoint contract stays
/// the single-shot one: every segment lands in the same checkpoint summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPlan {
    /// `single` never splits; `segmented` only when the prefix cannot fit.
    pub mode: &'static str,
    /// Consecutive half-open message ranges, in order.
    pub segments: Vec<(usize, usize)>,
}

/// Partition the selected messages by how much one summarizer call may carry.
///
/// A call ends when the excerpt characters it would send reach `segment_chars`,
/// or when it reaches `per_pass` messages if the caller set a cap (`0` leaves the
/// budget alone to decide). Counting messages is what made a million-token
/// history plan hundreds of calls: a capped excerpt of eight messages is a few
/// thousand tokens, and the summarizer's own window is a hundred times that.
pub fn plan_summary_by_size(sizes: &[usize], per_pass: usize, segment_chars: usize) -> SummaryPlan {
    let single = || SummaryPlan {
        mode: "single",
        segments: vec![(0, sizes.len())],
    };
    let fits_one_call = sizes.iter().sum::<usize>() <= segment_chars.max(1)
        && (per_pass == 0 || sizes.len() <= per_pass);
    if sizes.is_empty() || fits_one_call {
        return single();
    }
    let mut segments = Vec::new();
    let mut start = 0;
    let mut carried = 0;
    for (index, size) in sizes.iter().enumerate() {
        let over_budget = carried > 0 && carried + size > segment_chars;
        let over_cap = per_pass > 0 && index - start >= per_pass;
        if over_budget || over_cap {
            segments.push((start, index));
            start = index;
            carried = 0;
        }
        carried += size;
    }
    if start < sizes.len() {
        segments.push((start, sizes.len()));
    }
    SummaryPlan {
        mode: if segments.len() <= 1 {
            "single"
        } else {
            "segmented"
        },
        segments: if segments.len() <= 1 {
            single().segments
        } else {
            segments
        },
    }
}

/// Partition `prefix` messages into summarizer passes of at most `per_pass`
/// messages. A non-positive budget means "cannot summarize"; the plan stays
/// empty rather than guessing.
pub fn plan_summary(prefix: usize, per_pass: usize) -> SummaryPlan {
    if per_pass == 0 {
        return SummaryPlan {
            mode: "segmented",
            segments: Vec::new(),
        };
    }
    if prefix <= per_pass {
        return SummaryPlan {
            mode: "single",
            segments: vec![(0, prefix)],
        };
    }
    let mut segments = Vec::new();
    let mut start = 0;
    while start < prefix {
        let end = (start + per_pass).min(prefix);
        segments.push((start, end));
        start = end;
    }
    SummaryPlan {
        mode: "segmented",
        segments,
    }
}

/// Gate ②: the provider's own count wins; absence or refusal falls back to
/// the estimate. A counting failure never fails the turn — this function
/// cannot fail by construction.
pub fn resolve_pending_tokens(counted: crate::provider::CountTokens, chars: usize) -> u64 {
    match counted {
        crate::provider::CountTokens::Counted(tokens) => tokens,
        _ => estimate_tokens(chars),
    }
}

impl Store {
    /// The highest committed message seq, recorded inside each usage event so a
    /// later turn knows exactly which messages that report already counted.
    pub async fn max_message_seq(&self, session: &str) -> Result<u64, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM engine_message WHERE session_id=?1",
                [&session],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// The gate-① anchor: the newest usage event's normalized context tokens
    /// plus the highest message seq that report counted. An event without the
    /// marker predates the anchor, so it reports `None` rather than inventing
    /// one; the trigger stays off until a later turn reports a marked usage.
    ///
    /// A report from before the newest compaction no longer describes what the
    /// next request carries: the pass replaced that history with a summary.
    /// Keeping it as the anchor made the turn after a pass plan against the
    /// pre-compaction count, so the operator read the old figure beside a much
    /// smaller request and every later decision inherited it.
    pub async fn context_anchor(&self, session: &str) -> Result<Option<(u64, u64)>, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let cut: u64 = conn.query_row(
                "SELECT COALESCE((SELECT created_seq FROM engine_compaction WHERE session_id=?1 ORDER BY created_seq DESC LIMIT 1),0)",
                [&session],
                |row| row.get(0),
            )?;
            let data: Option<String> = conn
                .query_row(
                    "SELECT data FROM engine_event WHERE session_id=?1 AND kind='usage' AND seq>?2 ORDER BY seq DESC LIMIT 1",
                    rusqlite::params![session, cut],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(data) = data else { return Ok(None) };
            let event = serde_json::from_str::<Value>(&data)?;
            let Some(through) = event["context_message_seq"].as_u64() else {
                return Ok(None);
            };
            let usage = &event["usage"];
            // The prompt is the counted history. Every normalizer reports
            // `inputTokens` as the whole request (cache reads included), and
            // `through` is the last message that request carried. The reply
            // lands after `through`, so `pending_context` already prices it;
            // adding `outputTokens` here charged every turn's own reply twice
            // and compacted that much early.
            let input = usage["inputTokens"].as_u64().unwrap_or(0);
            Ok(Some((input, through)))
        })
        .await
    }

    /// The messages committed after the anchor's count, plus their serialized
    /// chars: exactly the delta the provider still has to count.
    pub async fn pending_context(
        &self,
        session: &str,
        counted_through: u64,
    ) -> Result<(Vec<Message>, u64), Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT role,content,recorded_at_ms FROM engine_message WHERE session_id=?1 AND seq>?2 AND reverted=0 AND quarantined=0 ORDER BY seq",
            )?;
            let mut rows = stmt.query(rusqlite::params![session, counted_through])?;
            let mut chars = 0usize;
            let mut messages = Vec::new();
            while let Some(row) = rows.next()? {
                let content: String = row.get(1)?;
                chars += content.len();
                messages.push(Message {
                    role: row.get(0)?,
                    content: serde_json::from_str(&content)?,
                    checkpoint: None,
                    recorded_at_ms: row.get(2)?,
                });
            }
            Ok((messages, chars as u64))
        })
        .await
    }

    /// The tokens the newest checkpoint's carried summary occupies in the next
    /// request, so a scoped threshold can discount it. Zero means no checkpoint
    /// stands, or the operator cleared it with an empty cut.
    pub async fn carried_prefix_tokens(&self, session: &str) -> Result<u64, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let summary: Option<String> = conn
                .query_row(
                    "SELECT summary FROM engine_compaction WHERE session_id=?1 ORDER BY created_seq DESC LIMIT 1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(match summary {
                Some(summary) if !summary.is_empty() => {
                    estimate_tokens(
                        crate::history::history_summary_text(&summary).chars().count(),
                    )
                }
                _ => 0,
            })
        })
        .await
    }

    /// Normalized context anchor from the newest `usage` event, or `None`
    /// before the first provider turn.
    pub async fn last_context_tokens(&self, session: &str) -> Result<Option<u64>, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let data: Option<String> = conn
                .query_row(
                    "SELECT data FROM engine_event WHERE session_id=?1 AND kind='usage' ORDER BY seq DESC LIMIT 1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(data) = data else { return Ok(None) };
            let usage = serde_json::from_str::<serde_json::Value>(&data)?;
            let usage = &usage["usage"];
            let input = usage["inputTokens"].as_u64().unwrap_or(0);
            let output = usage["outputTokens"].as_u64().unwrap_or(0);
            Ok(Some(input.saturating_add(output)))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two spellings a host may use for the scoped measurement, and the
    /// default that keeps every existing configuration counting the whole
    /// request.
    #[test]
    fn a_scoped_prefix_and_a_retention_target_survive_the_wire() {
        let scoped = ContextLimits::from_wire(
            r#"{"window":1000,"ratio":0.5,"headroom":10,"perPass":0,"prefixScope":"bodyAfterPrefix","retention":"keep failing test names"}"#,
        )
        .unwrap();
        assert_eq!(scoped.prefix_scope, PrefixScope::BodyAfterPrefix);
        assert_eq!(scoped.retention.as_deref(), Some("keep failing test names"));

        let snake = ContextLimits::from_wire(
            r#"{"window":1000,"ratio":0.5,"headroom":10,"perPass":0,"prefixScope":"body_after_prefix"}"#,
        )
        .unwrap();
        assert_eq!(snake.prefix_scope, PrefixScope::BodyAfterPrefix);

        let bare =
            ContextLimits::from_wire(r#"{"window":1000,"ratio":0.5,"headroom":10,"perPass":0}"#)
                .unwrap();
        assert_eq!(bare.prefix_scope, PrefixScope::Total);
        assert_eq!(bare.retention, None);
    }

    /// The fabricated claim a weak model produces to stop an autonomous
    /// stretch — in either language — reads as a claim, while an ordinary
    /// mention of the window, its figures, or unrelated waiting does not.
    #[test]
    fn an_exhausted_window_claim_matches_without_firing_on_plain_mentions() {
        assert!(claims_context_exhaustion(
            "这个会话的上下文窗口用完了。继续方式：开一个新会话即可无缝接上。"
        ));
        assert!(claims_context_exhaustion(
            "The context window is exhausted; please open a new session to continue."
        ));
        assert!(claims_context_exhaustion(
            "本会话上下文到此为止，剩余余量已不足以安全地再开一个多步任务。"
        ));
        assert!(!claims_context_exhaustion(
            "自动压缩是存在的，触发线是窗口的 90%，当前占用 61%。"
        ));
        assert!(!claims_context_exhaustion(
            "The context window carries 534800 of 868900 tokens (61%)."
        ));
        assert!(!claims_context_exhaustion(
            "Waiting for the device to recover."
        ));
        assert!(!claims_context_exhaustion(
            "任务完成，四个 PR 已合入 main。"
        ));
    }

    /// A summarizer call the interrupt aborted reports a transport error. Read
    /// as a summarizer failure it would feed gate ⑤, so an operator stopping a
    /// turn three times closed compaction for the rest of the Session.
    #[test]
    fn an_aborted_summarizer_call_is_an_interrupt_not_a_failure() {
        let cancel = tokio_util::sync::CancellationToken::new();
        assert!(matches!(
            settled(Err("transport closed".into()), &cancel),
            Segment::Answered(Err(_))
        ));
        cancel.cancel();
        assert!(matches!(
            settled(Err("transport closed".into()), &cancel),
            Segment::Cancelled
        ));
        assert!(matches!(
            settled(Ok("summary".into()), &cancel),
            Segment::Answered(Ok(_))
        ));
    }

    fn text_message(role: &str, body: &str) -> Message {
        Message {
            role: role.into(),
            content: json!([{"type": "text", "text": body}]),
            checkpoint: None,
            recorded_at_ms: None,
        }
    }

    fn tool_result_message(id: &str, body: &str) -> Message {
        Message {
            role: "user".into(),
            content: json!([{"type": "tool_result", "tool_use_id": id, "content": body}]),
            checkpoint: None,
            recorded_at_ms: None,
        }
    }

    // The guard exists so a request the endpoint already refused is never sent
    // again unchanged: it keeps the operator's prompt and the newest work.
    #[test]
    fn trimming_keeps_the_prompt_and_drops_oldest_turns() {
        let mut messages = vec![
            text_message("user", &"old question ".repeat(200)),
            text_message("assistant", &"old answer ".repeat(200)),
            text_message("user", "newest question"),
        ];
        let stats = trim_history_to_fit(&mut messages, 200);
        assert_eq!(stats.messages, 2);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content[0]["text"], "newest question");
        assert!(stats.after_bytes < stats.before_bytes);
    }

    #[test]
    fn trimming_never_leaves_an_orphan_tool_result() {
        let mut messages = vec![
            text_message("user", "ask"),
            text_message("assistant", &"call ".repeat(400)),
            tool_result_message("call_1", &"output ".repeat(400)),
            text_message("assistant", &"more ".repeat(400)),
            text_message("user", "continue"),
        ];
        let stats = trim_history_to_fit(&mut messages, 400);
        assert!(stats.messages > 0);
        assert!(!carries_tool_result(&messages[0]));
        assert_eq!(messages.last().unwrap().content[0]["text"], "continue");
    }

    // A request must start on a clean user turn: a leading assistant message, or a
    // tool result whose call was dropped, is rejected by the wire format.
    #[test]
    fn trimming_starts_the_request_on_a_clean_user_turn() {
        let mut messages = vec![
            text_message("user", &"ask ".repeat(300)),
            text_message("assistant", &"answer ".repeat(300)),
            tool_result_message("call_9", &"output ".repeat(300)),
            text_message("user", &"next ask ".repeat(300)),
            text_message("assistant", &"next answer ".repeat(300)),
            text_message("user", "continue"),
        ];
        let stats = trim_history_to_fit(&mut messages, 300);
        assert!(stats.messages > 0);
        assert_eq!(messages[0].role, "user");
        assert!(!carries_tool_result(&messages[0]));
        assert_eq!(messages.last().unwrap().content[0]["text"], "continue");
    }

    #[test]
    fn trimming_leaves_a_fitting_request_alone() {
        let mut messages = vec![
            text_message("user", "small"),
            text_message("assistant", "ok"),
        ];
        let stats = trim_history_to_fit(&mut messages, 10_000);
        assert_eq!(stats.messages, 0);
        assert_eq!(messages.len(), 2);
    }

    /// The default budget only fires where it was chosen to: an old payload is
    /// elided down to the per-payload budget, the newest turn keeps its output,
    /// and a request that stays under the total target loses nothing.
    #[test]
    fn the_default_prune_budget_touches_only_what_no_longer_fits() {
        let payload = |text: &str| Message {
            role: "user".into(),
            content: json!([{"type":"tool_result","tool_use_id":"call","content":text.repeat(40_000)}]),
            checkpoint: None,
            recorded_at_ms: None,
        };
        let mut messages = vec![payload("a"), payload("b")];
        let stats = prune_history_with_stats(
            &mut messages,
            DEFAULT_PRUNE_CHARS,
            1,
            Some(DEFAULT_PRUNE_TOTAL_CHARS),
        );
        assert_eq!(
            stats.payloads, 0,
            "80 KB of candidates fit the 256 KB target"
        );
        assert_eq!(
            messages[0].content[0]["content"].as_str().unwrap().len(),
            40_000
        );

        let mut messages = vec![payload("a"), payload("b")];
        let stats = prune_history_with_stats(&mut messages, DEFAULT_PRUNE_CHARS, 1, Some(20_000));
        assert!(stats.payloads >= 1, "the excess is covered oldest-first");
        assert_eq!(
            messages[1].content[0]["content"].as_str().unwrap().len(),
            40_000,
            "the newest turn keeps its output"
        );
        assert!(
            messages[0].content[0]["content"].as_str().unwrap().len() < 40_000,
            "the old payload is the one that shrinks"
        );
    }

    #[test]
    fn prune_elides_oversized_tool_output_and_protects_recent_turns() {
        let mut messages = vec![
            Message {
                role: "user".into(),
                content: json!([{"type":"tool_result","content":"x".repeat(4000)}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
            Message {
                role: "assistant".into(),
                content: json!([{"type":"text","text":"ok"}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
            Message {
                role: "user".into(),
                content: json!([{"type":"tool_result","content":"y".repeat(4000)}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
        ];
        // The newest user message keeps its output; the older one is elided.
        let (elided, dropped) = prune_history(&mut messages, 1000, 1, None);
        assert_eq!(elided, 1);
        assert!(dropped > 0);
        let kept = messages[2].content[0]["content"].as_str().unwrap();
        assert_eq!(kept.len(), 4000);
        let pruned = messages[0].content[0]["content"].as_str().unwrap();
        assert!(pruned.contains("elided by context management"), "{pruned}");
        assert!(pruned.len() < 4000);

        let payload = |text: &str| Message {
            role: "user".into(),
            content: json!([{"type":"tool_result","tool_use_id":"call","content":text.repeat(4000)}]),
            checkpoint: None,
            recorded_at_ms: None,
        };

        // Three 4000-byte candidates sum to 12000 against a total target of
        // 8000, so selection elides oldest-first only until the excess of 4000
        // is covered: the newest candidate keeps its output whole.
        let mut messages = vec![payload("a"), payload("b"), payload("c")];
        let (elided, dropped) = prune_history(&mut messages, 1000, 0, Some(8000));
        assert_eq!(elided, 2);
        assert!(dropped >= 4000, "{dropped}");
        for index in [0, 1] {
            let text = messages[index].content[0]["content"].as_str().unwrap();
            assert!(text.contains("elided by context management"), "{index}");
        }
        assert_eq!(
            messages[2].content[0]["content"].as_str().unwrap().len(),
            4000
        );

        // A target the candidates already fit elides nothing at all.
        let mut fitting = vec![payload("a"), payload("b"), payload("c")];
        assert_eq!(prune_history(&mut fitting, 1000, 0, Some(20000)), (0, 0));
        assert!(fitting.iter().all(|message| message.content[0]["content"]
            .as_str()
            .unwrap()
            .len()
            == 4000));

        // Within budget nothing is touched, and protecting every turn is a no-op.
        let mut small = vec![Message {
            role: "user".into(),
            content: json!([{"type":"tool_result","content":"z".repeat(50)}]),
            checkpoint: None,
            recorded_at_ms: None,
        }];
        assert_eq!(prune_history(&mut small, 1000, 0, None), (0, 0));
        assert_eq!(prune_history(&mut messages, 1000, 3, None), (0, 0));
    }

    #[test]
    fn threshold_prefers_the_later_bound() {
        // The explicit headroom raises the trigger when it reserves more than
        // the window ratio does.
        let budget = Budget {
            window: Some(100_000),
            ratio: 0.3,
            headroom: 40_000,
        };
        assert_eq!(budget.threshold(), 40_000);
        // The ratio raises it when the headroom is the smaller reserve.
        let budget = Budget {
            window: Some(100_000),
            ratio: 0.8,
            headroom: 50_000,
        };
        assert_eq!(budget.threshold(), 80_000);
        // A headroom sized for a large model must not put the trigger past a
        // small window: the reserve never takes more than half, so a 6K model
        // still compacts before its wall instead of never reaching the line.
        let budget = Budget {
            window: Some(6_000),
            ratio: 0.8,
            headroom: 112_048,
        };
        assert!(
            budget.threshold() < 6_000,
            "the trigger has to be reachable: {}",
            budget.threshold()
        );
        assert_eq!(budget.threshold(), 4_800);
        // An unknown window is the explicit headroom alone.
        let budget = Budget {
            window: None,
            ratio: 0.8,
            headroom: 50_000,
        };
        assert_eq!(budget.threshold(), 50_000);
    }

    /// Configuration is not trusted to be reachable arithmetic: a ratio above
    /// one would arm the trigger past the window, where no request can take the
    /// history, and automatic compaction would never fire.
    #[test]
    fn a_ratio_above_one_cannot_arm_the_trigger_past_the_window() {
        let budget = Budget {
            window: Some(100_000),
            ratio: 2.0,
            headroom: 10_000,
        };
        assert_eq!(budget.threshold(), 100_000);
        let budget = Budget {
            window: Some(100_000),
            ratio: 1.0,
            headroom: 10_000,
        };
        assert_eq!(budget.threshold(), 100_000);
        let budget = Budget {
            window: Some(100_000),
            ratio: -0.5,
            headroom: 10_000,
        };
        assert_eq!(budget.threshold(), 10_000, "the reserve is the floor");
    }

    /// `context: 0` means the route states no window. A zero threshold used to
    /// read as "already crossed", compacting on every settled turn.
    #[test]
    fn a_zero_window_states_no_budget_instead_of_always_crossing() {
        let budget = Budget {
            window: Some(0),
            ratio: 0.88,
            headroom: 62_048,
        };
        assert_eq!(budget.threshold(), 0);
        assert!(!budget.should_compact(Some(1_000_000), 0));
    }

    #[test]
    fn a_missing_provider_count_is_an_estimate_not_a_veto() {
        let budget = Budget {
            window: Some(1_000),
            ratio: 0.8,
            headroom: 100,
        };
        // Nothing counted yet: the pending estimate alone decides, so a Session
        // whose provider reports no usage still compacts instead of growing to
        // the hard limit with the trigger idle.
        assert!(!budget.should_compact(None, 799));
        assert!(budget.should_compact(None, 800));
        assert!(!budget.should_compact(Some(700), 50));
        assert!(budget.should_compact(Some(700), 100));
    }

    #[test]
    fn an_unknown_window_plans_against_the_explicit_headroom() {
        let budget = Budget {
            window: None,
            ratio: 0.8,
            headroom: 50_000,
        };
        assert_eq!(budget.threshold(), 50_000);
        assert!(!budget.should_compact(None, 49_999));
        assert!(budget.should_compact(None, 50_000));
        let budget = Budget {
            window: None,
            ratio: 0.8,
            headroom: 1,
        };
        assert!(!budget.should_compact(None, 0));
        assert!(budget.should_compact(None, 1));
    }

    #[test]
    fn summary_plans_stay_single_until_the_prefix_cannot_fit() {
        assert_eq!(
            plan_summary(4, 8),
            SummaryPlan {
                mode: "single",
                segments: vec![(0, 4)]
            }
        );
        assert_eq!(
            plan_summary(8, 8),
            SummaryPlan {
                mode: "single",
                segments: vec![(0, 8)]
            }
        );
        assert_eq!(
            plan_summary(9, 4),
            SummaryPlan {
                mode: "segmented",
                segments: vec![(0, 4), (4, 8), (8, 9)]
            }
        );
        assert_eq!(
            plan_summary(0, 4),
            SummaryPlan {
                mode: "single",
                segments: vec![(0, 0)]
            }
        );
        assert_eq!(
            plan_summary(9, 0),
            SummaryPlan {
                mode: "segmented",
                segments: vec![]
            }
        );
    }

    /// A summarizer that misses its first deadline is retried with the wider
    /// one instead of failing the pass: failing it used to discard every
    /// segment summarized before it and count against gate ⑤.
    #[tokio::test]
    async fn a_missed_deadline_is_retried_with_the_wider_one() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let deadlines = Deadlines {
            first: Duration::from_millis(10),
            retry: Duration::from_secs(5),
        };
        let mut calls = 0;
        let outcome = summarize_segment(
            &mut |_body: String| {
                calls += 1;
                let late = calls == 1;
                async move {
                    if late {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                    }
                    Ok("summary".to_string())
                }
            },
            "condense this",
            &cancel,
            deadlines,
        )
        .await;
        assert!(
            matches!(outcome, Segment::Answered(Ok(_))),
            "the retry answers"
        );
        assert_eq!(calls, 2, "exactly one retry after the missed deadline");
    }

    /// The flat twenty seconds made every real pass look slow: the history that
    /// needs compacting is the history one call cannot read in twenty seconds.
    /// The budget follows the slice instead, and stays bounded at both ends.
    #[test]
    fn a_summarizer_deadline_follows_the_slice_it_reads() {
        assert_eq!(deadlines_for(0).first, SUMMARY_TIMEOUT);
        assert_eq!(deadlines_for(0).retry, SUMMARY_TIMEOUT);
        let slice = deadlines_for(300_000);
        assert!(
            slice.first > SUMMARY_TIMEOUT && slice.first < SUMMARY_CEILING,
            "a real slice earns more than the floor and less than the ceiling: {:?}",
            slice.first
        );
        assert_eq!(
            deadlines_for(usize::MAX).first,
            SUMMARY_CEILING,
            "a slice nobody could read is still bounded"
        );
    }

    /// A summarizer that never answers spends both deadlines and is reported as
    /// slow, which is the only path that feeds gate ⑤.
    #[tokio::test]
    async fn a_summarizer_that_never_answers_is_slow() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let deadlines = Deadlines {
            first: Duration::from_millis(5),
            retry: Duration::from_millis(5),
        };
        let mut calls = 0;
        let outcome = summarize_segment(
            &mut |_body: String| {
                calls += 1;
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok("late".to_string())
                }
            },
            "condense this",
            &cancel,
            deadlines,
        )
        .await;
        assert!(matches!(outcome, Segment::Slow));
        assert_eq!(calls, 2, "both deadlines were spent");
    }

    /// An interrupt during the first deadline ends the pass instead of spending
    /// the retry: the operator already asked for the turn to stop.
    #[tokio::test]
    async fn an_interrupt_during_the_first_deadline_cancels_the_retry() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let interrupt = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            interrupt.cancel();
        });
        let deadlines = Deadlines {
            first: Duration::from_millis(30),
            retry: Duration::from_secs(5),
        };
        let mut calls = 0;
        let outcome = summarize_segment(
            &mut |_body: String| {
                calls += 1;
                async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok("summary".to_string())
                }
            },
            "condense this",
            &cancel,
            deadlines,
        )
        .await;
        assert!(matches!(outcome, Segment::Cancelled));
        assert_eq!(calls, 1, "a cancelled pass does not spend the retry");
    }

    /// A pass is planned by what one summarizer call can carry, not by how many
    /// messages there are: eight messages of a capped excerpt are a few thousand
    /// tokens, and the summarizer's own window is a hundred times that.
    #[test]
    fn a_pass_is_planned_by_size_not_by_message_count() {
        let sizes = [100usize, 100, 100, 100];
        assert_eq!(
            plan_summary_by_size(&sizes, 0, 250),
            SummaryPlan {
                mode: "segmented",
                segments: vec![(0, 2), (2, 4)]
            }
        );
        // The same history in one call when the budget can carry it.
        assert_eq!(
            plan_summary_by_size(&sizes, 0, 10_000),
            SummaryPlan {
                mode: "single",
                segments: vec![(0, 4)]
            }
        );
        // An operator's message cap still applies when they set one.
        assert_eq!(
            plan_summary_by_size(&sizes, 1, 10_000),
            SummaryPlan {
                mode: "segmented",
                segments: vec![(0, 1), (1, 2), (2, 3), (3, 4)]
            }
        );
        // An oversized message is its own call: a segment is never empty.
        assert_eq!(
            plan_summary_by_size(&[500, 10], 0, 100),
            SummaryPlan {
                mode: "segmented",
                segments: vec![(0, 1), (1, 2)]
            }
        );
    }

    /// The exact last request is the most faithful summary input a Session can
    /// give, so an over-window one is trimmed — tool output first, by the handle
    /// `recall` restores — instead of being refused.
    #[test]
    fn an_over_window_request_is_trimmed_rather_than_refused() {
        let mut messages = vec![
            text_message("user", "look at this"),
            Message {
                role: "user".into(),
                content: json!([{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": "x".repeat(30_000),
                }]),
                checkpoint: None,
                recorded_at_ms: None,
            },
        ];
        // Thirty thousand characters are about ten thousand tokens, which a
        // two-thousand-token window cannot hold.
        assert!(fit_request(&mut messages, 2_000, 0));
        let trimmed = messages[1].content[0]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(trimmed.contains("recall"), "{trimmed}");
        // Prose the summarizer cannot do without stays: a request whose own text
        // is too large is refused rather than silently emptied.
        let mut prose = vec![text_message("user", &"y".repeat(30_000))];
        assert!(!fit_request(&mut prose, 2_000, 0));
    }

    #[test]
    fn wire_form_parses_camel_case_and_rejects_garbage() {
        let limits = ContextLimits::from_wire(
            r#"{"window":200000,"ratio":0.85,"headroom":20000,"perPass":50}"#,
        )
        .unwrap();
        assert_eq!(limits.window, 200_000);
        assert_eq!((limits.ratio * 100.0) as u64, 85);
        assert_eq!(limits.headroom, 20_000);
        assert_eq!(limits.per_pass, 50);
        assert!(ContextLimits::from_wire(r#"{"ratio":0.5}"#).is_err());
    }

    #[test]
    fn breaker_trips_after_the_threshold_and_resets_explicitly() {
        let mut breaker = Breaker::new(3);
        assert!(breaker.allows());
        breaker.rejected(100_000);
        breaker.rejected(100_000);
        assert!(breaker.allows());
        breaker.rejected(100_000);
        assert!(!breaker.allows());
        breaker.rejected(100_000);
        assert!(!breaker.allows());
        breaker.reset();
        assert!(breaker.allows());
    }

    // The streak is consecutive: a pass that committed means the summarizer is
    // answering, so the next rejection must not trip a breaker that just
    // succeeded — and `idle()` has to say so, or the stored breaker outlives
    // the failure it described.
    #[test]
    fn a_committed_pass_clears_the_failure_streak() {
        let mut breaker = Breaker::new(3);
        for _ in 0..3 {
            breaker.rejected(100_000);
        }
        assert!(!breaker.allows());
        assert!(breaker.allows_at(200_000));
        breaker.committed();
        assert!(breaker.idle());
        assert!(breaker.allows());
        breaker.rejected(200_000);
        assert!(breaker.allows(), "one failure is not a streak");
    }

    // A summarizer that failed once must not disable compaction for the rest of
    // the Session: the history keeps growing, and a permanently tripped breaker
    // only guarantees that it reaches the endpoint's hard ceiling, where every
    // request is refused and the Session cannot be continued.
    #[test]
    fn a_tripped_breaker_opens_one_attempt_once_the_history_grows() {
        let mut breaker = Breaker::new(3);
        for _ in 0..3 {
            breaker.rejected(1_000_000);
        }
        assert!(!breaker.allows());
        // The same size would fail the same way, so the trip still holds.
        assert!(!breaker.allows_at(1_000_000));
        assert!(!breaker.allows_at(1_050_000));
        // A tenth larger is a different attempt, and it opens exactly one.
        assert!(breaker.allows_at(1_100_000));
        assert!(breaker.allows());
        breaker.rejected(1_100_000);
        breaker.rejected(1_100_000);
        breaker.rejected(1_100_000);
        assert!(!breaker.allows_at(1_100_000));
    }

    // Once a Session's requests are bounded deterministically the history never
    // grows past the size that failed, so size alone cannot bring a repaired
    // summarizer back: the wait has to.
    #[test]
    fn a_tripped_breaker_retries_a_repaired_summarizer_after_the_wait() {
        let mut breaker = Breaker::new(2);
        breaker.rejected(1_000);
        breaker.rejected(1_000);
        assert!(!breaker.allows());
        for _ in 0..3 {
            assert!(!breaker.allows_at(1_000));
        }
        assert!(breaker.allows_at(1_000), "the wait opens one attempt");
        breaker.rejected(1_000);
        assert!(!breaker.allows());
        assert!(!breaker.allows_at(1_000), "one open attempt per window");
    }

    #[test]
    fn tool_output_pruning_protects_recent_and_bounds_old() {
        assert_eq!(prune_tool_output(100, 200, true), Prune::Keep);
        assert_eq!(prune_tool_output(100, 200, false), Prune::Keep);
        assert_eq!(
            prune_tool_output(100_000, 4_000, false),
            Prune::Elide {
                keep_head: 2_000,
                keep_tail: 1_000
            }
        );
        // Tiny budgets keep the recognizable minimum on both ends.
        assert_eq!(
            prune_tool_output(1_000, 10, false),
            Prune::Elide {
                keep_head: 64,
                keep_tail: 32
            }
        );
    }

    #[test]
    fn provider_count_wins_and_every_other_outcome_falls_back() {
        use crate::provider::CountTokens;
        assert_eq!(
            resolve_pending_tokens(CountTokens::Counted(4321), 9_000),
            4321
        );
        assert_eq!(
            resolve_pending_tokens(CountTokens::Unsupported, 9_000),
            estimate_tokens(9_000)
        );
        assert_eq!(
            resolve_pending_tokens(
                CountTokens::Failed("count refused: 429 Too Many Requests".into()),
                3
            ),
            1
        );
    }

    #[test]
    fn char_estimate_rounds_up() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(6), 2);
    }

    #[test]
    fn summarizer_body_bounds_messages_and_names_archived_outputs() {
        let messages = vec![
            Message {
                role: "user".into(),
                content: json!([{"type": "text", "text": "x".repeat(150)}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
            Message {
                role: "user".into(),
                content: json!([{"type": "tool_result", "tool_use_id": "call_big", "content": "y".repeat(10_000)}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
        ];
        let body = summarizer_body(&messages, 100);
        assert!(body.contains("- user:"), "{body}");
        assert!(body.contains("…[truncated]"), "{body}");
        assert!(
            body.contains(r#"recall {"tool_use_id":"call_big"} to restore"#),
            "{body}"
        );
        // The archived payload's body never reaches the summarizer.
        assert!(body.len() < 1_000, "{body}");
    }

    #[test]
    fn durable_state_block_carries_goal_and_open_tasks_verbatim() {
        let state = json!({
            "goal": {"value": {"objective": "land the fix", "status": "active"}},
            "todos": {"value": {"todos": [
                {"content": "write test", "status": "pending", "priority": "high"},
                {"content": "done item", "status": "completed", "priority": "low"},
                {"content": "dropped item", "status": "cancelled", "priority": "low"}
            ]}}
        });
        let block = durable_state_block(&state);
        assert!(
            block.contains("Objective: land the fix (status: active)"),
            "{block}"
        );
        assert!(block.contains("- [ ] write test"), "{block}");
        assert!(!block.contains("done item"), "{block}");
        assert!(!block.contains("dropped item"), "{block}");

        // No durable state, no block: explicit compact keeps its plain summary.
        assert_eq!(durable_state_block(&json!({})), "");
    }

    #[tokio::test]
    async fn checkpoint_summary_keeps_the_durable_block_within_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("engine.db")).await.unwrap();
        store
            .admit(crate::protocol::Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "task".into(),
                delivery: crate::protocol::Delivery::Steer,
            })
            .await
            .unwrap();
        store
            .update_state(
                "s",
                "op-goal",
                crate::state::Mutation {
                    kind: "goal",
                    value: json!({"objective": "finish", "status": "active"}),
                    expected: None,
                },
            )
            .await
            .unwrap();
        let huge = vec!["z".repeat(MAX_CHECKPOINT_CHARS); 2];
        let summary = checkpoint_summary(&store, &huge, "s").await;
        assert!(summary.len() <= MAX_CHECKPOINT_CHARS, "{}", summary.len());
        assert!(
            summary.ends_with("finish (status: active)") || summary.contains("Objective: finish"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn anchor_reads_the_newest_usage_event() {
        use crate::protocol::{Delivery, Input};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("engine.db")).await.unwrap();
        assert_eq!(store.last_context_tokens("s").await.unwrap(), None);
        store
            .admit(Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "old task".into(),
                delivery: Delivery::Steer,
            })
            .await
            .unwrap();
        store.promote("s", true).await.unwrap();
        store
            .record(
                "s",
                "usage",
                json!({"run_id":"run","step":0,"usage":{"inputTokens":900,"outputTokens":100}}),
            )
            .await
            .unwrap();
        store
            .record(
                "s",
                "usage",
                json!({"run_id":"run","step":1,"usage":{"inputTokens":1_500,"outputTokens":50}}),
            )
            .await
            .unwrap();
        store
            .record("s", "run.started", json!({"run_id":"run"}))
            .await
            .unwrap();
        assert_eq!(store.last_context_tokens("s").await.unwrap(), Some(1_550));
        let budget = Budget {
            window: Some(2_000),
            ratio: 0.8,
            headroom: 0,
        };
        assert!(budget.should_compact(
            store.last_context_tokens("s").await.unwrap(),
            estimate_tokens(1_000)
        ));
    }

    /// A report from before the newest compaction no longer describes what the
    /// next request carries: the pass replaced that history with a summary.
    /// It used to stay the anchor, so the turn after a pass planned against the
    /// pre-pass count and every client showing it read the pass as a no-op.
    #[tokio::test]
    async fn a_report_from_before_the_newest_compaction_is_not_the_anchor() {
        use crate::protocol::{Delivery, Input};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("engine.db")).await.unwrap();
        store
            .admit(Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "old task".into(),
                delivery: Delivery::Steer,
            })
            .await
            .unwrap();
        store.promote("s", true).await.unwrap();
        let boundary = store
            .message(
                "s",
                "assistant",
                json!([{"type":"text","text":"completed task"}]),
            )
            .await
            .unwrap()
            .seq;
        store
            .record(
                "s",
                "usage",
                json!({"run_id":"run","step":0,"usage":{"inputTokens":600_000,"outputTokens":10},"context_message_seq":boundary}),
            )
            .await
            .unwrap();
        store
            .compact("s", "cut", boundary, "summary".into())
            .await
            .unwrap();
        assert_eq!(
            store.context_anchor("s").await.unwrap(),
            None,
            "the pre-compaction report cannot describe the compacted prefix"
        );
        store
            .record(
                "s",
                "usage",
                json!({"run_id":"run","step":1,"usage":{"inputTokens":21_000,"outputTokens":10},"context_message_seq":boundary}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.context_anchor("s").await.unwrap(),
            Some((21_000, boundary))
        );
    }
}
