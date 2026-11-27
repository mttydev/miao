//! Subscription quota for providers that meter a coding plan.
//!
//! The sidebar's billed usage answers "what did this Session cost"; it cannot
//! answer "how much of the plan is left". Three routes publish that meter: a
//! Z.ai/智谱 coding plan under `/api/monitor/usage/quota/limit`, opencode-go
//! under `/zen/go/v1/usage`, and Command Code under `/alpha/billing/credits`.
//! All are read with the same stored credential the provider turns already
//! use, and none is required for a turn to run.

use crate::credential::Credential;
use serde_json::{json, Value};
use std::time::Duration;

const ZAI_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const OPENCODE_GO_USAGE_PATH: &str = "/zen/go/v1/usage";
const COMMANDCODE_CREDITS_PATH: &str = "/alpha/billing/credits";
const COMMANDCODE_SUMMARY_PATH: &str = "/alpha/usage/summary";
const COMMANDCODE_SUBSCRIPTION_PATH: &str = "/alpha/billing/subscriptions";

#[derive(Clone, Copy)]
enum Flavor {
    Zai,
    OpenCodeGo,
    CommandCode,
}

/// Where a stored integration's plan meter lives. A credential for any other
/// integration has no endpoint to ask, so the sidebar simply has no row.
fn plan_of(integration: &str) -> Option<(&'static str, Flavor)> {
    match integration {
        // The CN coding plan (open.bigmodel.cn) and the international Z.ai
        // account expose the same monitor shape on their own hosts.
        "zhipuai-coding-plan" => Some(("https://open.bigmodel.cn", Flavor::Zai)),
        "zai" | "z-ai" => Some(("https://api.z.ai", Flavor::Zai)),
        "opencode-go" => Some(("https://opencode.ai", Flavor::OpenCodeGo)),
        "commandcode" => Some(("https://api.commandcode.ai", Flavor::CommandCode)),
        _ => None,
    }
}

/// Whether a stored integration has a plan meter this build can read.
pub(crate) fn metered(integration: &str) -> bool {
    plan_of(integration).is_some()
}

/// Fetch one stored credential's plan windows. `None` means the account has no
/// plan to meter, the endpoint answered a shape this build does not know, or
/// the network failed: an absence is a missing row, not a turn failure.
pub(crate) async fn fetch(
    client: &reqwest::Client,
    integration: &str,
    credential: &Credential,
) -> Option<Value> {
    let (host, flavor) = plan_of(integration)?;
    let bearer = format!("Bearer {}", credential.secret());
    match flavor {
        Flavor::Zai => {
            let response = meter_request(
                client,
                format!("{host}{ZAI_QUOTA_PATH}"),
                credential.secret().to_owned(),
                false,
            )
            .send()
            .await
            .ok()?;
            if !response.status().is_success() {
                return None;
            }
            parse_zai(integration, &response.json().await.ok()?)
        }
        Flavor::OpenCodeGo => {
            let response = meter_request(
                client,
                format!("{host}{OPENCODE_GO_USAGE_PATH}"),
                bearer,
                true,
            )
            .send()
            .await
            .ok()?;
            if !response.status().is_success() {
                return None;
            }
            parse_opencode_go(integration, &response.json().await.ok()?)
        }
        Flavor::CommandCode => {
            // The windows, the monthly pool and the day it renews live on
            // three documents. The meter carries the 5-hour and weekly usage,
            // the summary the month's spend, and the subscription the period's
            // end; the monthly cap is the spend plus what remains.
            let credits = meter_request(
                client,
                format!("{host}{COMMANDCODE_CREDITS_PATH}"),
                bearer.clone(),
                false,
            )
            .send()
            .await
            .ok()?;
            if !credits.status().is_success() {
                return None;
            }
            let credits: Value = credits.json().await.ok()?;
            let summary = commandcode_optional(
                client,
                format!("{host}{COMMANDCODE_SUMMARY_PATH}"),
                bearer.clone(),
            )
            .await;
            let subscription = commandcode_optional(
                client,
                format!("{host}{COMMANDCODE_SUBSCRIPTION_PATH}"),
                bearer,
            )
            .await;
            parse_commandcode(
                integration,
                &credits,
                summary.as_ref(),
                subscription.as_ref(),
            )
        }
    }
}

fn meter_request(
    client: &reqwest::Client,
    url: String,
    authorization: String,
    session: bool,
) -> reqwest::RequestBuilder {
    let request = client
        .get(url)
        .timeout(Duration::from_secs(8))
        .header("accept", "application/json")
        .header("authorization", authorization);
    if session {
        // The gateway attributes a background poll outside any conversation;
        // without this header it answers as an anonymous client.
        return request.header("x-opencode-session", "miao-quota");
    }
    request
}

async fn commandcode_optional(
    client: &reqwest::Client,
    url: String,
    authorization: String,
) -> Option<Value> {
    let response = meter_request(client, url, authorization, false)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The single-document parsers below read one shape each; Command Code needs
/// three documents, so it is parsed by [`parse_commandcode`] instead.
#[cfg(test)]
fn parse(integration: &str, flavor: Flavor, body: &Value) -> Option<Value> {
    match flavor {
        Flavor::Zai => parse_zai(integration, body),
        Flavor::OpenCodeGo => parse_opencode_go(integration, body),
        // Command Code reads three documents; see `parse_commandcode`.
        Flavor::CommandCode => None,
    }
}

fn parse_commandcode(
    integration: &str,
    credits: &Value,
    summary: Option<&Value>,
    subscription: Option<&Value>,
) -> Option<Value> {
    let limits = &credits["windowLimits"];
    let mut windows = [
        (&limits["fiveHour"], "5h", "5 Hour"),
        (&limits["weekly"], "7d", "Weekly"),
    ]
    .into_iter()
    .filter_map(|(window, id, label)| commandcode_window(window, id, label))
    .collect::<Vec<_>>();
    // The monthly pool is not a rolling window: the summary says what the
    // period has spent and the meter says what is left, so their sum is the
    // cap even when the plan's name (Max 10x/20x) does not say which it is.
    let used = summary
        .and_then(|summary| summary["totalMonthlyCredits"].as_f64())
        .filter(|used| *used >= 0.0);
    let remaining = credits["credits"]["monthlyCredits"]
        .as_f64()
        .filter(|remaining| *remaining >= 0.0);
    if let (Some(used), Some(remaining)) = (used, remaining) {
        let cap = used + remaining;
        if cap > 0.0 {
            windows.push(window_json(
                "monthly",
                "Monthly",
                "credits",
                Some(used),
                Some(cap),
                used / cap * 100.0,
                subscription.and_then(commandcode_period_end),
            ));
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(json!({
        "provider": integration,
        "plan": subscription
            .and_then(|subscription| subscription["data"]["planId"].as_str())
            .map(|plan| plan.strip_prefix("individual-").unwrap_or(plan))
            .unwrap_or_default(),
        "windows": windows,
        "fetched_at_ms": crate::approval::now_ms(),
    }))
}

fn commandcode_window(window: &Value, id: &str, label: &str) -> Option<Value> {
    let used = window["used"].as_f64()?;
    let cap = window["cap"].as_f64().filter(|cap| *cap > 0.0)?;
    let percent = used / cap * 100.0;
    // `resetAt` is 0 until the window opens on the first request; that is "not
    // started", not "resets now".
    let resets_at_ms = window["resetAt"].as_u64().filter(|at| *at > 0);
    let status = if window["exceeded"] == json!(true) {
        "exhausted"
    } else {
        status(percent)
    };
    Some(json!({
        "id": id,
        "label": label,
        "unit": "credits",
        "used": used,
        "limit": cap,
        "percent": percent.clamp(0.0, 100.0),
        "status": status,
        "resets_at_ms": resets_at_ms,
    }))
}

fn commandcode_period_end(subscription: &Value) -> Option<u64> {
    let end = subscription["data"]["currentPeriodEnd"].as_str()?;
    chrono::DateTime::parse_from_rfc3339(end)
        .ok()
        .map(|end| end.timestamp_millis().max(0) as u64)
}

fn parse_zai(integration: &str, body: &Value) -> Option<Value> {
    // `success: false` also means "this account has no coding plan", which is
    // an absence rather than an error to show.
    if body["success"] != json!(true) {
        return None;
    }
    let data = &body["data"];
    let windows = data["limits"]
        .as_array()?
        .iter()
        .filter_map(zai_window)
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(json!({
        "provider": integration,
        "plan": data["level"].as_str().unwrap_or_default(),
        "windows": windows,
        "fetched_at_ms": crate::approval::now_ms(),
    }))
}

fn zai_window(limit: &Value) -> Option<Value> {
    let (unit, label) = match limit["type"].as_str()? {
        "TOKENS_LIMIT" => ("tokens", "tokens"),
        "TIME_LIMIT" => ("requests", "requests"),
        "CREDIT_LIMIT" => ("credits", "credits"),
        _ => return None,
    };
    let count = limit["number"]
        .as_u64()
        .filter(|count| *count > 0)
        .unwrap_or(1);
    let (id, window) = match limit["unit"].as_u64() {
        Some(3) => (
            format!("{count}h"),
            format!("{count} {}", plural(count, "Hour")),
        ),
        Some(4) => (
            format!("{count}d"),
            format!("{count} {}", plural(count, "Day")),
        ),
        Some(5) => (
            "monthly".to_owned(),
            if count == 1 {
                "Monthly".to_owned()
            } else {
                format!("{count} Months")
            },
        ),
        Some(6) => ("1w".to_owned(), "Weekly".to_owned()),
        _ => ("quota".to_owned(), "Quota".to_owned()),
    };
    let used = limit["currentValue"].as_f64();
    let cap = limit["usage"].as_f64().filter(|cap| *cap > 0.0);
    // The account page paints its own percentage; prefer that reading so the
    // sidebar and the site agree to the digit. The ratio is the fallback for
    // responses that omit it.
    let percent = match limit["percentage"].as_f64() {
        Some(percent) => percent,
        None => match (used, cap) {
            (Some(used), Some(cap)) => used / cap * 100.0,
            _ => return None,
        },
    };
    Some(window_json(
        &id,
        &format!("{label} · {window}"),
        unit,
        used,
        cap,
        percent,
        limit["nextResetTime"].as_u64().map(to_millis),
    ))
}

fn parse_opencode_go(integration: &str, body: &Value) -> Option<Value> {
    let usage = body["usage"].as_object()?;
    let windows = [
        ("rolling", "5h", "5 Hour"),
        ("weekly", "7d", "Weekly"),
        ("monthly", "monthly", "Monthly"),
    ]
    .iter()
    .filter_map(|(key, id, label)| {
        let window = &usage[*key];
        let percent = window["percent"].as_f64()?;
        let status = if window["status"] == json!("rate-limited") {
            "exhausted"
        } else {
            status(percent)
        };
        Some(json!({
            "id": id,
            "label": label,
            "unit": "percent",
            "used": percent,
            "limit": 100.0,
            "percent": percent,
            "status": status,
            "resets_at_ms": window["resetsAt"]
                .as_str()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.timestamp_millis().max(0) as u64),
        }))
    })
    .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(json!({
        "provider": integration,
        "plan": "",
        "windows": windows,
        "fetched_at_ms": crate::approval::now_ms(),
    }))
}

fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        unit.to_owned()
    } else {
        format!("{unit}s")
    }
}

/// Z.ai's epochs are seconds when they are not already milliseconds.
fn to_millis(value: u64) -> u64 {
    if value > 1_000_000_000_000 {
        value
    } else {
        value.saturating_mul(1_000)
    }
}

fn status(percent: f64) -> &'static str {
    if percent >= 100.0 {
        "exhausted"
    } else if percent >= 80.0 {
        "warning"
    } else {
        "ok"
    }
}

fn window_json(
    id: &str,
    label: &str,
    unit: &str,
    used: Option<f64>,
    cap: Option<f64>,
    percent: f64,
    resets_at_ms: Option<u64>,
) -> Value {
    let percent = percent.clamp(0.0, 100.0);
    json!({
        "id": id,
        "label": label,
        "unit": unit,
        "used": used,
        "limit": cap,
        "percent": percent,
        "status": status(percent),
        "resets_at_ms": resets_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zai_coding_plan_windows_keep_their_own_units() {
        let body = json!({
            "success": true,
            "data": {
                "level": "lite",
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":12000.0,"currentValue":1438.0,"percentage":12,"nextResetTime":1760000000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":60000.0,"currentValue":0.0,"percentage":0}
                ]
            }
        });
        let report = parse("zhipuai-coding-plan", Flavor::Zai, &body).expect("report");
        assert_eq!(report["plan"], "lite");
        let windows = report["windows"].as_array().unwrap();
        assert_eq!(windows[0]["id"], "5h");
        assert_eq!(windows[0]["label"], "credits · 5 Hours");
        assert_eq!(windows[0]["percent"], 12.0);
        assert_eq!(windows[0]["resets_at_ms"], 1_760_000_000_000u64);
        assert_eq!(windows[1]["id"], "1w");
        assert_eq!(windows[1]["label"], "credits · Weekly");
    }

    #[test]
    fn an_account_without_a_coding_plan_has_no_report() {
        let body = json!({"success": false, "code": 500, "msg": "当前用户不存在coding plan"});
        assert!(parse("zhipuai-coding-plan", Flavor::Zai, &body).is_none());
    }

    #[test]
    fn opencode_go_reads_the_three_windows_and_their_reset() {
        let body = json!({"usage": {
            "rolling": {"status":"ok","percent":41,"resetsAt":"2026-11-17T10:00:00Z"},
            "weekly": {"status":"rate-limited","percent":100,"resetsAt":"2026-11-20T00:00:00Z"},
            "monthly": {"status":"ok","percent":8,"resetsAt":"2026-12-01T00:00:00Z"}
        }});
        let report = parse("opencode-go", Flavor::OpenCodeGo, &body).expect("report");
        let windows = report["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0]["id"], "5h");
        assert_eq!(windows[0]["status"], "ok");
        assert_eq!(windows[1]["status"], "exhausted");
        assert_eq!(windows[2]["percent"], 8.0);
        assert_eq!(
            windows[0]["resets_at_ms"],
            chrono::DateTime::parse_from_rfc3339("2026-11-17T10:00:00Z")
                .unwrap()
                .timestamp_millis() as u64
        );
    }

    #[test]
    fn commandcode_reads_both_rolling_windows_and_the_monthly_pool() {
        let credits = json!({
            "credits": {"monthlyCredits": 60.694672673, "purchasedCredits": 0.0},
            "windowLimits": {
                "limited": true,
                "fiveHour": {"used": 0.0, "cap": 45.0, "exceeded": false, "resetAt": 0},
                "weekly": {"used": 89.305327327, "cap": 90.0, "exceeded": false, "resetAt": 1_795_911_830_405u64}
            }
        });
        let summary = json!({"totalMonthlyCredits": 89.305327327});
        let subscription = json!({"data": {
            "planId": "individual-max",
            "currentPeriodEnd": "2026-12-21T16:11:07.000Z"
        }});
        let report =
            parse_commandcode("commandcode", &credits, Some(&summary), Some(&subscription))
                .expect("report");
        assert_eq!(report["plan"], "max");
        let windows = report["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0]["id"], "5h");
        assert_eq!(windows[0]["used"], 0.0);
        assert_eq!(windows[0]["resets_at_ms"], Value::Null);
        assert_eq!(windows[1]["id"], "7d");
        assert!((windows[1]["percent"].as_f64().unwrap() - 99.228).abs() < 0.01);
        assert_eq!(windows[1]["status"], "warning");
        assert_eq!(windows[1]["resets_at_ms"], 1_795_911_830_405u64);
        assert_eq!(windows[2]["id"], "monthly");
        assert!((windows[2]["percent"].as_f64().unwrap() - 59.537).abs() < 0.01);
        assert_eq!(
            windows[2]["resets_at_ms"],
            chrono::DateTime::parse_from_rfc3339("2026-12-21T16:11:07Z")
                .unwrap()
                .timestamp_millis() as u64
        );
    }

    #[test]
    fn commandcode_still_paints_its_windows_when_the_month_is_unknown() {
        let credits =
            json!({"windowLimits": {"fiveHour": {"used": 3.0, "cap": 45.0, "resetAt": 0}}});
        let report = parse_commandcode("commandcode", &credits, None, None).expect("report");
        assert_eq!(report["windows"].as_array().unwrap().len(), 1);
        assert_eq!(report["plan"], "");
    }

    #[test]
    fn an_unknown_integration_is_not_asked() {
        assert!(plan_of("openai").is_none());
        assert!(plan_of("zhipuai-coding-plan").is_some());
        assert!(plan_of("opencode-go").is_some());
        assert!(plan_of("commandcode").is_some());
    }
}
