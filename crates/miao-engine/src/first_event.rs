use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

const REQUEST_BYTES_PER_SECOND: f64 = 32.0 * 1024.0;
const MAX_REQUEST_ALLOWANCE: Duration = Duration::from_secs(30 * 60);

fn milliseconds(key: &str, fallback: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(fallback)
}

fn seconds(key: &str, fallback: f64) -> Duration {
    Duration::try_from_secs_f64(milliseconds(key, fallback) / 1000.0)
        .unwrap_or(Duration::try_from_secs_f64(fallback / 1000.0).unwrap_or(Duration::ZERO))
}

/// Layered per-attempt liveness bounds, mirroring the session runner's phases.
///
/// `header` bounds dispatch through response headers and is widened by the
/// encoded request size; `byte_idle` bounds silence between body bytes, with
/// protocol keep-alives counting as bytes; `event` bounds the wait for a
/// canonical event and is re-armed by semantic events, never keep-alive traffic. Setting
/// `MIAO_LLM_FIRST_BYTE_TIMEOUT_MS=0` disables the whole watchdog, as it did
/// when the phases shared one deadline.
#[derive(Clone, Copy)]
pub(crate) struct Watchdogs {
    header: Option<Instant>,
    pub(crate) byte_idle: Duration,
    pub(crate) event: Option<Instant>,
    pub(crate) event_idle: Duration,
}

impl Watchdogs {
    /// Anchors every phase at provider dispatch. Callers without a readable
    /// request body pass `0`, which simply skips the size allowance.
    pub(crate) fn at(request_bytes: usize) -> Self {
        let started_at = Instant::now();
        let first_byte_ms = milliseconds("MIAO_LLM_FIRST_BYTE_TIMEOUT_MS", 180_000.0);
        if first_byte_ms == 0.0 {
            return Self {
                header: None,
                byte_idle: Duration::ZERO,
                event: None,
                event_idle: Duration::ZERO,
            };
        }
        let allowance = Duration::try_from_secs_f64(
            (request_bytes as f64 / REQUEST_BYTES_PER_SECOND).floor(), // seconds
        )
        .unwrap_or(Duration::ZERO)
        .min(MAX_REQUEST_ALLOWANCE);
        let event_idle = seconds("MIAO_LLM_EVENT_IDLE_TIMEOUT_MS", 300_000.0);
        Self {
            header: Some(
                started_at + seconds("MIAO_LLM_FIRST_BYTE_TIMEOUT_MS", 180_000.0) + allowance,
            ),
            byte_idle: seconds("MIAO_LLM_STREAM_IDLE_TIMEOUT_MS", 180_000.0),
            event: (event_idle > Duration::ZERO).then(|| started_at + event_idle),
            event_idle,
        }
    }

    /// Only a semantic event extends the rolling event-idle bound.
    pub(crate) fn next_event_deadline(&self) -> Option<Instant> {
        (self.event_idle > Duration::ZERO).then(|| Instant::now() + self.event_idle)
    }

    pub(crate) fn event_timeout(&self, received: bool) -> crate::provider::ProviderError {
        if !received {
            return crate::provider::ProviderError::Transport(format!(
                "provider produced no stream event for {}ms (set MIAO_LLM_EVENT_IDLE_TIMEOUT_MS to adjust, 0 disables)",
                self.event_idle.as_millis(),
            ));
        }
        crate::provider::ProviderError::Stream(format!(
            "provider produced no stream event for {}ms (set MIAO_LLM_EVENT_IDLE_TIMEOUT_MS to adjust, 0 disables)",
            self.event_idle.as_millis(),
        ))
    }

    /// Deadline for response headers, before any body byte arrives.
    pub(crate) fn header_deadline(&self) -> Option<Instant> {
        self.header
    }
}

/// A watchdog that never fires, for streams that carry no anchored attempt.
impl Default for Watchdogs {
    fn default() -> Self {
        Self {
            header: None,
            byte_idle: Duration::ZERO,
            event: None,
            event_idle: Duration::ZERO,
        }
    }
}

/// Whether a frame proves the reply has started producing output.
///
/// This decides the classification of a later silence, so it has to mean
/// "content arrived", not "a frame arrived". A gateway that opens a stream with
/// its assistant role and a usage object and then goes quiet for three minutes
/// has produced nothing; classifying that silence as a stalled stream made the
/// turn terminal and unrepeatable, while the same silence before the first
/// frame was replayed. The answer now comes from [`crate::delta::normalize`],
/// the engine's own definition of published content, so a frame that carries no
/// text, reasoning or tool input can never arm the terminal path.
pub(crate) fn semantic_event(event: &Value) -> bool {
    if event["type"] == "ping" {
        return false;
    }
    // A provider that speaks in the engine's own delta vocabulary has no raw
    // frame to read, so a delta carrying payload is itself the proof. Control
    // frames (`provider.retry`, `provider.attempt`, `provider.window`) never
    // name a delta, and a handshake `content_start` carries none.
    if event["kind"]
        .as_str()
        .is_some_and(|kind| kind.contains("delta"))
        && ["text", "delta", "arguments"]
            .iter()
            .any(|field| event[*field].as_str().is_some_and(|text| !text.is_empty()))
    {
        return true;
    }
    // Releasing the event deadline is a different question from proving the
    // reply started: `message_start` (and its siblings) is a canonical event,
    // so it keeps a healthy turn's clock rolling, while only real content can
    // make a later silence terminal. See `content` and `provider::read_stream`.
    content(event) || lifecycle(event)
}

/// A frame that declares a reply has begun. Canonical, so it re-arms the event
/// deadline, but it carries no output.
pub(crate) fn lifecycle(event: &Value) -> bool {
    matches!(
        event["type"].as_str(),
        Some("message_start" | "response.created" | "content_block_start" | "message_delta")
    )
}

/// A frame whose only payload is framing: the assistant role, an empty delta,
/// a finish marker, or a usage block with no content beside it.
pub(crate) fn content(event: &Value) -> bool {
    if !crate::delta::normalize(event).is_empty() {
        return true;
    }
    // Providers that announce a complete message instead of streaming it (a
    // whole tool call, a whole reply) carry no delta at all.
    let array = |value: Option<&Value>| {
        value
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    };
    array(event.get("content"))
        || array(event.get("candidates"))
        || array(event.get("output"))
        || event["message"]["content"].is_array()
}

#[cfg(test)]
mod tests {
    use super::semantic_event;
    use serde_json::json;

    /// A frame that names the assistant role, or carries only a usage block,
    /// has produced nothing. Counting it as output is what turned a gateway's
    /// silence into a terminal, unrepeatable failure while the same silence
    /// before any frame was replayed.
    #[test]
    fn only_real_content_counts_as_the_first_event() {
        let silence_only = [
            // The role handshake an OpenAI-compatible gateway opens with.
            json!({"choices":[{"index":0,"delta":{"role":"assistant"}}]}),
            // The same, spelled with explicit nulls and empty fields.
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":null,"content":"","tool_calls":[]},"finish_reason":null}]}),
            // Keep-alives and an empty choice list.
            json!({"type":"ping"}),
            json!({"choices":[],"usage":null}),
            // A usage block with no content beside it. Some gateways send one
            // of these before the answer starts.
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":0}}),
            json!({"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}],"usage":{"prompt_tokens":10}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}),
        ];
        for frame in &silence_only {
            assert!(
                !semantic_event(frame),
                "framing must not count as output: {frame}"
            );
        }
    }

    /// A lifecycle frame releases the event deadline but is not output.
    ///
    /// The two questions are separate: `message_start` keeps a healthy turn's
    /// clock rolling, while a gateway that sends it and then goes quiet has
    /// produced nothing — so its silence stays replayable instead of becoming a
    /// terminal stalled stream.
    #[test]
    fn a_lifecycle_frame_releases_the_deadline_without_counting_as_output() {
        for frame in [
            json!({"type":"message_start","message":{"usage":{"input_tokens":9}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"response.created"}),
        ] {
            assert!(
                super::semantic_event(&frame),
                "a canonical event re-arms the deadline: {frame}"
            );
            assert!(!super::content(&frame), "but it carries no output: {frame}");
        }
    }

    /// Content in any of the shapes the engine publishes is output.
    #[test]
    fn content_in_every_protocol_counts_as_the_first_event() {
        let output = [
            json!({"choices":[{"index":0,"delta":{"content":"hel"}}]}),
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"thinking"}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":""}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]}}]}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"y"}}),
            json!({"candidates":[{"index":0,"content":{"parts":[{"text":"z"}]}}]}),
        ];
        for frame in &output {
            assert!(
                semantic_event(frame),
                "content must count as output: {frame}"
            );
        }
    }
    use super::Watchdogs;
    use std::time::Duration;
    use tokio::time::Instant;

    /// Deadline widths measured from a moment just before construction; the
    /// constructor only runs a few env reads in between, so a small tolerance
    /// keeps this exact without exposing internal state.
    fn within(actual: Duration, seconds: u64) -> bool {
        let expected = Duration::from_secs(seconds);
        actual >= expected && actual - expected <= Duration::from_millis(50)
    }

    #[test]
    fn env_deadlines_layer_and_zero_first_byte_disables_everything() {
        std::env::set_var("MIAO_LLM_FIRST_BYTE_TIMEOUT_MS", "1000");
        std::env::set_var("MIAO_LLM_STREAM_IDLE_TIMEOUT_MS", "2000");
        std::env::set_var("MIAO_LLM_EVENT_IDLE_TIMEOUT_MS", "4000");
        let anchor = Instant::now();
        let watchdogs = Watchdogs::at(64 * 1024);
        assert!(within(watchdogs.header_deadline().unwrap() - anchor, 3));
        assert_eq!(watchdogs.byte_idle, Duration::from_secs(2));
        assert!(within(watchdogs.event.unwrap() - anchor, 4));

        std::env::set_var("MIAO_LLM_FIRST_BYTE_TIMEOUT_MS", "0");
        let watchdogs = Watchdogs::at(0);
        assert!(watchdogs.header_deadline().is_none());
        assert_eq!(watchdogs.byte_idle, Duration::ZERO);
        assert!(watchdogs.event.is_none());

        std::env::remove_var("MIAO_LLM_FIRST_BYTE_TIMEOUT_MS");
        std::env::remove_var("MIAO_LLM_STREAM_IDLE_TIMEOUT_MS");
        std::env::remove_var("MIAO_LLM_EVENT_IDLE_TIMEOUT_MS");
    }
}
