//! Per-prompt reasoning effort, picked from the variants a model registers.
//!
//! The product carries reasoning effort as model variants: `low`, `medium` and
//! `high` are separate catalog entries of one route, each its own Session pin.
//! A Session starts with none, and the operator may pin one. With `auto_effort`
//! on, this module names a variant for the prompt about to be sent, so a short
//! question does not spend a reasoning budget while a refactor asks for one.
//!
//! The decision is local and deterministic: no model call, no network, and no
//! opinion at all about names outside the ladder, so a route that registers
//! `thinking` or `fast` is left exactly as the operator pinned it.

/// The effort ladder from least to most. Only these names are ranked; anything
/// else is a variant this module does not understand and therefore ignores.
const LADDER: [&str; 5] = ["minimal", "low", "medium", "high", "max"];

/// Work that is worth a reasoning budget: it names a change rather than a
/// question about the code in front of the operator.
const HARD_WORDS: [&str; 18] = [
    "refactor",
    "重构",
    "migrate",
    "迁移",
    "architecture",
    "架构",
    "implement",
    "实现",
    "debug",
    "排查",
    "root cause",
    "根因",
    "concurrency",
    "并发",
    "optimize",
    "优化",
    "design",
    "设计",
];

/// The variant name to pin for `prompt`, or `None` when this route offers no
/// meaningful choice. A route counts as offering one only when it registers at
/// least two ladder names: with one, or with names outside the ladder, there is
/// nothing to decide.
pub fn pick(prompt: &str, variants: &[String]) -> Option<String> {
    // The spelling the route registered is what the engine looks the variant up
    // by, so the pick is one of those strings rather than a ladder rung.
    let mut available: Vec<&str> = variants
        .iter()
        .map(String::as_str)
        .filter(|variant| rank_of(variant).is_some())
        .collect();
    available.sort_unstable_by_key(|name| rank(name));
    available.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if available.len() < 2 {
        return None;
    }
    Some(nearest(&available, target(prompt)).to_string())
}

/// The rung this prompt asks for. A long prompt, a diff-sized one, or one that
/// names a change is `high`; a short question that names no change is `low`;
/// everything else sits in the middle.
fn target(prompt: &str) -> &'static str {
    if is_hard(prompt) {
        return "high";
    }
    if prompt.chars().count() <= 60 {
        return "low";
    }
    "medium"
}

fn is_hard(prompt: &str) -> bool {
    if prompt.chars().count() >= 400 || prompt.matches("```").count() >= 2 {
        return true;
    }
    let lowered = prompt.to_lowercase();
    HARD_WORDS.iter().any(|word| lowered.contains(word))
}

/// The closest registered rung, preferring the higher one when two are equally
/// close: spending a little more reasoning is the cheaper mistake.
fn nearest<'a>(available: &[&'a str], target: &str) -> &'a str {
    let want = rank(target);
    available
        .iter()
        .copied()
        .min_by_key(|name| (rank(name).abs_diff(want), std::cmp::Reverse(rank(name))))
        .expect("callers pass a non-empty ladder")
}

fn rank(name: &str) -> usize {
    rank_of(name).unwrap_or(LADDER.len())
}

fn rank_of(name: &str) -> Option<usize> {
    LADDER
        .iter()
        .position(|rung| rung.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// Long enough to sit in the middle, and free of every `HARD_WORDS` entry.
    const MIDDLING: &str =
        "把这一段里的变量名统一一下，重复的段落合并掉，然后再检查一遍相邻函数是否还有重复的逻辑，最后把所有受影响的测试都跑一遍确认没有回归，并整理提交说明。";

    #[test]
    fn a_change_asks_for_the_highest_registered_rung() {
        let variants = names(&["low", "medium", "high"]);
        assert_eq!(
            pick(
                "请重构 crates/miao-engine/src/runtime.rs 的预算结算",
                &variants
            )
            .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn a_short_question_asks_for_the_lowest() {
        let variants = names(&["low", "medium", "high"]);
        assert_eq!(pick("what is 2+2?", &variants).as_deref(), Some("low"));
    }

    #[test]
    fn a_middling_prompt_sits_in_the_middle() {
        let variants = names(&["low", "medium", "high"]);
        assert_eq!(pick(MIDDLING, &variants).as_deref(), Some("medium"));
    }

    #[test]
    fn a_long_prompt_counts_as_work_even_without_a_keyword() {
        let variants = names(&["low", "high"]);
        let prompt = "the ".repeat(200);
        assert_eq!(pick(&prompt, &variants).as_deref(), Some("high"));
    }

    #[test]
    fn a_route_with_one_rung_or_unknown_names_is_left_alone() {
        assert_eq!(pick("重构一下", &names(&["high"])), None);
        assert_eq!(pick("重构一下", &names(&["thinking", "fast"])), None);
        assert_eq!(pick("重构一下", &[]), None);
    }

    #[test]
    fn the_closest_registered_rung_wins_and_ties_go_higher() {
        // `medium` is absent, and both neighbours are one step away.
        let variants = names(&["low", "high"]);
        assert_eq!(pick("what is 2+2?", &variants).as_deref(), Some("low"));
        assert_eq!(pick(MIDDLING, &variants).as_deref(), Some("high"));
    }

    #[test]
    fn an_extra_rung_beyond_the_ladder_does_not_change_the_pick() {
        let variants = names(&["minimal", "low", "medium", "high", "max", "turbo"]);
        assert_eq!(pick("what is 2+2?", &variants).as_deref(), Some("low"));
    }

    #[test]
    fn case_does_not_decide_what_a_variant_is_called() {
        let variants = names(&["LOW", "High"]);
        assert_eq!(
            pick("重构一下 runtime 的结算", &variants).as_deref(),
            Some("High")
        );
    }
}
