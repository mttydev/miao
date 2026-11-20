//! The operator's language for the TUI chrome.
//!
//! The engine's replies follow the user's language on their own; the labels
//! around them (`in`/`out`/`cache`, `TODO`, the composer's status line) are
//! painted from this catalog. `tui.json`'s `language` key or `/lang` picks a
//! language explicitly; absent a choice, `LC_ALL`/`LC_MESSAGES`/`LANG` does,
//! and English stays the fallback.

/// Every chrome string the TUI paints, in the two languages it ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    /// The value `tui.json`'s `language` key and `/lang` accept, case-free.
    pub fn parse(value: &str) -> Option<Lang> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Lang::En),
            "zh" | "zh-cn" | "zh-hans" | "chinese" | "中文" => Some(Lang::Zh),
            _ => None,
        }
    }

    /// The ambient locale, read the way gettext does: `LC_ALL` wins, then
    /// `LC_MESSAGES`, then `LANG`. A `zh` prefix means Chinese and any other
    /// stated language means English — but a language-less value (`C`,
    /// `POSIX`, the Terminal.app default `C.UTF-8`) states nothing, and on
    /// macOS the system's own language answers before English does.
    pub fn from_env() -> Lang {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(value) = std::env::var(key) {
                if let Some(lang) = shell_language(value.trim()) {
                    return lang;
                }
            }
        }
        if let Some(lang) = platform_language() {
            return lang;
        }
        Lang::En
    }

    /// The spelling `/lang` and `tui.json` accept back.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// The name shown to the operator, in that language.
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }

    /// One chrome string. The English column stays byte-identical to what the
    /// code painted before the catalog existed, so tests that assert on
    /// painted text keep passing.
    pub fn t(self, key: &str) -> &'static str {
        let (en, zh) = match key {
            "activity" => ("ACTIVITY", "活动"),
            "activity.auxiliary" => (
                "auxiliary {0} call{1} · in {2} · out {3}",
                "辅助调用 {0} 次 · 入 {2} · 出 {3}",
            ),
            "activity.decisions" => ("decisions {0}", "决策 {0}"),
            "activity.hooks" => ("hooks {0} · {1} ms{2}", "钩子 {0} · {1} ms{2}"),
            "activity.hooks_failed" => (" · {0} failed", " · {0} 失败"),
            "activity.subagent_attempts" => (" · {0} attempt{1}", " · {0} 次尝试"),
            "activity.subagents" => (
                "subagents {0} · {1} turn{2} · in {3}{4}",
                "子代理 {0} · {1} 轮 · 入 {3}{4}",
            ),
            "cache" => ("cache", "缓存"),
            "changes" => ("changes", "变更"),
            "changes.row" => ("{0} {1}  +{2} −{3}", "{0} 个文件  +{2} −{3}"),
            "composer.ask" => ("Ask anything…", "随便问…"),
            "composer.run" => ("Run a command…", "运行命令…"),
            "composer.thinking" => ("Thinking", "思考中"),
            "composer.thinking_summary" => ("Thinking · {0}", "思考中 · {0}"),
            "context" => ("context", "上下文"),
            "footer.hide_sidebar" => ("hide sidebar", "隐藏侧栏"),
            "in" => ("in", "入"),
            "lang.auto" => (
                "Language now follows the system locale ({0})",
                "界面语言跟随系统（当前 {0}）",
            ),
            "lang.current" => (
                "Language: {0} — /lang en · /lang zh · /lang auto",
                "界面语言：{0} —— /lang en · /lang zh · /lang auto",
            ),
            "lang.invalid" => (
                "/lang accepts en, zh, or auto — got \"{0}\"",
                "/lang 只接受 en、zh 或 auto —— 收到 \"{0}\"",
            ),
            "lang.persist_failed" => (
                "Language switched, but writing the config failed: {0}",
                "语言已切换，但写入配置失败：{0}",
            ),
            "lang.set" => ("Language set to {0}", "界面语言已切换为 {0}"),
            "live.compacting" => ("Compacting context", "压缩上下文"),
            "live.compacting_segments" => (
                "Compacting context · segment {0} of {1}",
                "压缩上下文 · 第 {0}/{1} 段",
            ),
            "live.retrying" => ("Retrying · attempt {0}", "重试中 · 第 {0} 次"),
            "live.retrying_detail" => ("Retrying · attempt {0} · {1}", "重试中 · 第 {0} 次 · {1}"),
            "live.retrying_left" => (
                "Retrying · attempt {0} · {1} left",
                "重试中 · 第 {0} 次 · 剩 {1}",
            ),
            "live.retrying_left_detail" => (
                "Retrying · attempt {0} · {1} left · {2}",
                "重试中 · 第 {0} 次 · 剩 {1} · {2}",
            ),
            "live.waiting" => ("Waiting for model", "等待模型"),
            "live.waiting_reasoning" => (
                "Waiting for model · the model may reason before the first token",
                "等待模型 · 模型可能先推理再输出首字",
            ),
            "live.waiting_silent" => (
                "Waiting for model · no engine signal {0}",
                "等待模型 · 无引擎信号 {0}",
            ),
            "live.waiting_window" => (
                "Waiting for model · still inside the provider window · Esc interrupts",
                "等待模型 · 仍在服务商窗口内 · Esc 可中断",
            ),
            "mode" => ("mode", "模式"),
            "model" => ("model", "模型"),
            "out" => ("out", "出"),
            "session.usage" => ("SESSION · USAGE", "会话 · 用量"),
            "status.backlog" => ("Engine backlog · {0} queued", "引擎积压 · {0} 条排队"),
            "status.done" => ("Done", "已完成"),
            "status.esc" => (" · Esc interrupt", " · Esc 中断"),
            "status.esc_bare" => ("Esc interrupt", "Esc 中断"),
            "status.esc_send_bare" => ("Esc interrupt and send", "Esc 中断并发送"),
            "status.idle" => ("Idle", "空闲"),
            "status.needs_answer" => ("needs an answer", "需要回答"),
            "status.needs_answer_many" => {
                ("needs an answer · {0} waiting", "需要回答 · {0} 个等待中")
            }
            "status.needs_approval_many" => {
                ("needs approval · {0} waiting", "需要批准 · {0} 个等待中")
            }
            "status.needs_approval_one" => ("needs approval · {0}", "需要批准 · {0}"),
            "status.needs_you" => (
                "needs you · {0} approval{1} · {2} question{3}",
                "需要你处理 · {0} 项批准 · {2} 个问题",
            ),
            "status.paused" => ("paused: {0}", "已暂停：{0}"),
            "status.queued" => (" · {0} queued", " · {0} 条排队"),
            "status.queued_esc" => (
                " · {0} queued · Esc interrupt and send",
                " · {0} 条排队 · Esc 中断并发送",
            ),
            "status.running" => ("Running {0}", "运行 {0}"),
            "status.subagent_one" => (" · 1 subagent", " · 1 个子代理"),
            "status.subagents" => (" · {0} subagents", " · {0} 个子代理"),
            "status.worked_for" => ("Worked for {0}", "已工作 {0}"),
            "status.working" => ("Working", "工作中"),
            "todo.done" => ("TODO done {0}/{1}", "待办完成 {0}/{1}"),
            "todo.progress" => ("TODO {0}/{1}", "待办 {0}/{1}"),
            "todo.zero" => ("TODO 0/{0}", "待办 0/{0}"),
            "turns" => ("turns", "轮"),
            "usage.none" => ("no usage recorded", "暂无用量记录"),
            "usage.now" => ("now", "刚刚"),
            "usage.peak" => (" · peak {0} · {1}", " · 峰值 {0} · {1}"),
            "working_tree" => ("WORKING TREE", "工作区"),
            "workspace.24h" => ("24H · WORKSPACE", "24H · 工作区"),
            _ => ("", ""),
        };
        match self {
            Lang::En => en,
            Lang::Zh => zh,
        }
    }

    /// `t` with `{0}`-`{9}` slots, for lines whose word order differs between
    /// the languages. An argument a translation does not mention stays
    /// unused, and the slot tokens never survive into painted prose.
    pub fn tf(self, key: &str, args: &[&str]) -> String {
        let mut out = self.t(key).to_string();
        for (index, arg) in args.iter().enumerate() {
            out = out.replace(&format!("{{{index}}}"), arg);
        }
        out
    }
}

/// The language a locale value states: `zh_CN.UTF-8` speaks Chinese and
/// `en_US.UTF-8` English, while `C`, `POSIX` and `C.UTF-8` — what a Chinese
/// macOS exports into the shell — state no language at all.
fn shell_language(value: &str) -> Option<Lang> {
    let language = value.split(['.', '@']).next()?.trim();
    if language.is_empty() || language == "C" || language == "POSIX" {
        return None;
    }
    primary_language(language)
}

/// The part of a language tag that decides our vocabulary: `zh-Hans-CN` and
/// `zh_CN` both speak Chinese, `en-US` English.
fn primary_language(code: &str) -> Option<Lang> {
    match code.split(['-', '_']).next()?.to_ascii_lowercase().as_str() {
        "zh" => Some(Lang::Zh),
        "en" => Some(Lang::En),
        _ => None,
    }
}

/// The operating system records the operator's real language even when the
/// shell exports a language-less `C.UTF-8` — or nothing at all, the Windows
/// default — so the system answers when the environment cannot. Dispatched
/// at runtime so every reader stays compiled and tested on every platform.
fn platform_language() -> Option<Lang> {
    match std::env::consts::OS {
        "macos" => apple_platform_language(),
        "windows" => windows_platform_language(),
        "linux" => linux_platform_language(),
        _ => None,
    }
}

fn apple_platform_language() -> Option<Lang> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())?;
    apple_language(&output).or_else(|| {
        let output = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())?;
        apple_language(&output)
    })
}

/// The primary language from `defaults read` output: the languages list
/// starts with it (`"zh-Hans-CN",`) and the locale reads `zh_CN`.
fn apple_language(text: &str) -> Option<Lang> {
    let stated = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('"') || (line.len() > 2 && !line.contains(' ')))?;
    primary_language(stated.trim_matches(|c: char| c == '"' || c == ','))
}

fn windows_platform_language() -> Option<Lang> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Control Panel\International\User Profile",
            "/v",
            "Languages",
        ])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())?;
    windows_ui_language(&output)
}

/// `reg query` prints the UI-language list on the REG_MULTI_SZ line, ordered
/// as Windows uses it for display:
/// `    Languages    REG_MULTI_SZ    zh-CN en-US`.
fn windows_ui_language(output: &str) -> Option<Lang> {
    let line = output.lines().find(|line| line.contains("REG_MULTI_SZ"))?;
    let mut parts = line
        .split_whitespace()
        .skip_while(|token| *token != "REG_MULTI_SZ");
    parts.next()?;
    primary_language(parts.next()?)
}

fn linux_platform_language() -> Option<Lang> {
    let conf = std::fs::read_to_string("/etc/locale.conf").ok()?;
    linux_system_language(&conf)
}

/// systemd records the system locale in `/etc/locale.conf`, usually
/// `LANG=zh_CN.UTF-8`, mirrored from the desktop's language setting.
fn linux_system_language(conf: &str) -> Option<Lang> {
    let line = conf
        .lines()
        .find(|line| line.trim_start().starts_with("LANG="))?;
    primary_language(
        line.trim_start()
            .strip_prefix("LANG=")?
            .trim()
            .trim_matches('"'),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key both languages ship, so a new call site without a
    /// translation fails here instead of painting an empty label.
    const KEYS: &[&str] = &[
        "activity",
        "activity.auxiliary",
        "activity.decisions",
        "activity.hooks",
        "activity.hooks_failed",
        "activity.subagent_attempts",
        "activity.subagents",
        "cache",
        "changes",
        "changes.row",
        "composer.ask",
        "composer.run",
        "composer.thinking",
        "composer.thinking_summary",
        "context",
        "footer.hide_sidebar",
        "in",
        "lang.auto",
        "lang.current",
        "lang.invalid",
        "lang.persist_failed",
        "lang.set",
        "live.compacting",
        "live.compacting_segments",
        "live.retrying",
        "live.retrying_detail",
        "live.retrying_left",
        "live.retrying_left_detail",
        "live.waiting",
        "live.waiting_reasoning",
        "live.waiting_silent",
        "live.waiting_window",
        "mode",
        "model",
        "out",
        "session.usage",
        "status.backlog",
        "status.done",
        "status.esc",
        "status.esc_bare",
        "status.esc_send_bare",
        "status.idle",
        "status.needs_answer",
        "status.needs_answer_many",
        "status.needs_approval_many",
        "status.needs_approval_one",
        "status.needs_you",
        "status.paused",
        "status.queued",
        "status.queued_esc",
        "status.running",
        "status.subagent_one",
        "status.subagents",
        "status.worked_for",
        "status.working",
        "todo.done",
        "todo.progress",
        "todo.zero",
        "turns",
        "usage.none",
        "usage.now",
        "usage.peak",
        "working_tree",
        "workspace.24h",
    ];

    #[test]
    fn every_key_exists_in_both_languages() {
        for key in KEYS {
            assert!(!Lang::En.t(key).is_empty(), "{key} lost its English text");
            assert!(!Lang::Zh.t(key).is_empty(), "{key} lost its Chinese text");
        }
    }

    #[test]
    fn parse_accepts_the_documented_spellings() {
        assert_eq!(Lang::parse("en"), Some(Lang::En));
        assert_eq!(Lang::parse(" EN "), Some(Lang::En));
        assert_eq!(Lang::parse("zh-CN"), Some(Lang::Zh));
        assert_eq!(Lang::parse("中文"), Some(Lang::Zh));
        assert_eq!(Lang::parse("fr"), None);
    }

    #[test]
    fn a_languageless_shell_locale_states_nothing() {
        // The Terminal.app default on a Chinese macOS: no language stated.
        assert_eq!(shell_language("C.UTF-8"), None);
        assert_eq!(shell_language("C"), None);
        assert_eq!(shell_language("POSIX"), None);
        assert_eq!(shell_language(""), None);
        assert_eq!(shell_language("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(shell_language("en_US.UTF-8"), Some(Lang::En));
    }

    #[test]
    fn the_apple_language_list_names_the_primary_language() {
        assert_eq!(
            apple_language("(\n    \"zh-Hans-CN\"\n)"),
            Some(Lang::Zh),
            "the zh-Hans macOS answers Chinese"
        );
        assert_eq!(
            apple_language("(\n    \"en-US\",\n    \"zh-Hans-CN\"\n)"),
            Some(Lang::En),
            "the primary language leads"
        );
        assert_eq!(apple_language("zh_CN"), Some(Lang::Zh));
        assert_eq!(apple_language("nonsense"), None);
    }

    #[test]
    fn the_windows_ui_language_list_names_the_primary_language() {
        assert_eq!(
            windows_ui_language(
                "\r\nHKEY_CURRENT_USER\\Control Panel\\International\\User Profile\r\n    \
                 Languages    REG_MULTI_SZ    zh-CN\r\n"
            ),
            Some(Lang::Zh),
            "a Chinese Windows answers Chinese"
        );
        assert_eq!(
            windows_ui_language("    Languages    REG_MULTI_SZ    en-US zh-CN"),
            Some(Lang::En),
            "the primary display language leads"
        );
        assert_eq!(
            windows_ui_language("    Language    REG_SZ    zh-CN"),
            None,
            "other registry values state nothing"
        );
    }

    #[test]
    fn the_linux_system_locale_names_the_primary_language() {
        assert_eq!(
            linux_system_language("[Language]\nLANG=zh_CN.UTF-8\nLC_TIME=zh_CN.UTF-8\n"),
            Some(Lang::Zh),
            "a Chinese desktop's locale.conf answers Chinese"
        );
        assert_eq!(linux_system_language("LANG=en_US.UTF-8\n"), Some(Lang::En));
        assert_eq!(
            linux_system_language("LANG=C.UTF-8\n"),
            None,
            "a languageless system locale states nothing"
        );
        assert_eq!(linux_system_language("LC_TIME=zh_CN.UTF-8\n"), None);
    }

    #[test]
    fn slots_substitute_without_leaving_tokens_behind() {
        let line = Lang::Zh.tf("todo.progress", &["2", "5"]);
        assert_eq!(line, "待办 2/5");
        assert!(!line.contains('{'), "{line:?}");
        let line = Lang::En.tf(
            "activity.subagents",
            &["2", "3", "s", "1.2k", " · 1 attempt"],
        );
        assert_eq!(line, "subagents 2 · 3 turns · in 1.2k · 1 attempt");
    }
}
