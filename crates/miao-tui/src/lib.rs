/// The release build injects the product version; a source build falls back to
/// the crate's version. The title bar names it so the operator can tell which
/// build is running.
pub const VERSION: &str = match option_env!("MIAO_TUI_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod app;
pub mod banner;
pub mod commands;
pub mod config;
pub mod dialogs;
pub mod editor;
pub mod effort;
pub mod engine;
pub mod event;
pub mod highlight;
pub mod input;
pub mod keybinds;
pub mod lang;
pub mod markdown;
pub mod mtty_state;
pub mod pane;
pub mod platform;
pub mod saved_permissions;
pub mod scroll;
pub mod session;
pub mod state;
pub mod status;
pub mod terminal;
pub mod terminal_title;
pub mod theme;
pub mod tool;
pub mod transport;
pub mod update;
pub mod workspace_changes;
pub mod worktrees;

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::app::{App, Startup};
use crate::event::AppEvent;

/// What the process was asked to open at launch. Every field is optional; the
/// TypeScript CLI fills them from `--session`, `--continue`, and `--prompt`.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub workspace: PathBuf,
    pub db: Option<PathBuf>,
    pub model: Option<String>,
    /// The product's configuration directory, forwarded to the engine as
    /// `--config-dir` so policy and LSP derivation match the TS host.
    pub config_dir: Option<PathBuf>,
    /// The layered config sources the launcher resolved, encoded as the
    /// engine's `--config-sources` plan. `Some` supersedes `config_dir`: the
    /// plan carries the managed, global and project layers, so a project's
    /// permissions, sandbox, budget and `readScope` reach the engine.
    pub config_sources: Option<String>,
    pub session: Option<String>,
    pub continue_last: bool,
    pub prompt: Option<String>,
    /// `--fork`: copy the resumed Session into a fresh one instead of
    /// appending onto the original. Requires `--session` or `--continue`.
    pub fork: bool,
    /// `--agent <name>`: start the Session under that configuration agent.
    /// Forwarded to the engine as `--agent-name`, which resolves the merged
    /// configuration's `agents` entry into the default manifest.
    pub agent: Option<String>,
    /// `--auto`/`--yolo`/`--dangerously-skip-permissions`: forwarded to the
    /// engine as `--auto-approve`, where asks the policy raises settle through
    /// the deterministic classifier instead of a host approval.
    pub auto: bool,
    /// `--remote-host <control.json>`: serve this window's engine to approved
    /// devices. The sidecar stops with the window.
    pub remote_host: Option<PathBuf>,
}

/// 8 MiB: the engine's turn chain needs about 4 MiB on Windows, where the
/// default worker stack is 2 MiB (#2926).
const TURN_STACK: usize = 8 * 1024 * 1024;

pub fn run(launch: Launch) -> Result<(), Box<dyn Error>> {
    platform::job_control::foreground()?;
    let workspace = launch.workspace.clone();
    let loaded = theme::load(&workspace);
    for warning in &loaded.warnings {
        eprintln!("miao-tui: {warning}");
    }
    let (config, warnings) = config::load(&workspace);
    for warning in &warnings {
        eprintln!("miao-tui: {warning}");
    }
    let language = match config.language.as_str() {
        "auto" => lang::Lang::from_env(),
        value => lang::Lang::parse(value).unwrap_or_else(lang::Lang::from_env),
    };
    let startup = Startup {
        session: launch.session.clone(),
        continue_last: launch.continue_last,
        prompt: launch.prompt.clone(),
        fork: launch.fork,
        auto: launch.auto,
        ..Startup::default()
    };
    let mut app = App::new(
        workspace.clone(),
        loaded.theme,
        loaded.catalog,
        keybinds::Keymap::from_overrides(&config.keybinds),
        config.attention,
        startup,
    );
    // An upgrade swaps the launcher's symlink and leaves running processes on
    // their old build, so the strip says when a newer build is installed. It is
    // read once at startup: the operator acts on it by restarting.
    app.set_pending_update(update::pending());
    app.configure_terminal_title(
        state::preferences().terminal_title_enabled.unwrap_or(true),
        !std::env::var("MIAO_DISABLE_TERMINAL_TITLE")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
    );
    app.configure_scroll(config.scroll_speed, config.scroll_acceleration);
    app.configure_leader_timeout(config.leader_timeout_ms);
    app.configure_animations(config.animations);
    app.configure_auto_effort(config.auto_effort);
    app.configure_predict_next(config.predict_next);
    app.configure_language(language);
    let preferences = terminal::Preferences {
        mouse: config.mouse,
        cursor: config.cursor,
        focus_reporting: config.focus_reporting,
    };
    // Multi-thread, not current_thread: the UI loop parks on the render
    // thread, and engine tasks (configure, catalog, streaming pumps) must not
    // depend on that loop yielding. A parked current_thread executor starved
    // those tasks and the landing screen spun on its loader for minutes.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // The engine's turn chain runs here too, and needs more than the 2 MiB
        // worker default on Windows (#2926).
        .thread_stack_size(TURN_STACK)
        .build()?;
    let termination = {
        let _entered = runtime.enter();
        platform::signals::Signals::new()?
    };
    let mut restore = terminal::RestoreGuard::new();
    let mut terminal = terminal::init(preferences)?;
    terminal::install_panic_hook();
    let (bus, mut events) = event::bus();
    let result = runtime.block_on(drive(
        &mut terminal,
        &mut app,
        &mut events,
        bus,
        launch,
        preferences,
        termination,
    ));
    // Hand the screen back before writing the epilogue. The guard disarms here,
    // so the drop that covers early returns cannot restore the cursor again:
    // doing that after the epilogue made the shell paint its prompt over the
    // epilogue and the old prompt line.
    drop(terminal);
    restore.hand_back();
    if let Some(session) = app.session.as_ref() {
        // The restored screen cursor sits on the shell's old prompt line, so
        // the epilogue opens a line of its own instead of appending to it.
        print!("\n{}", session::epilogue(session));
        let _ = io::stdout().flush();
    }
    result
}

async fn drive(
    terminal: &mut terminal::Tui,
    app: &mut App,
    events: &mut event::Rx,
    bus: event::Bus,
    launch: Launch,
    preferences: terminal::Preferences,
    mut termination: platform::signals::Signals,
) -> Result<(), Box<dyn Error>> {
    let config = match launch.config_sources.as_deref() {
        Some(plan) => engine::ConfigSource::Plan(plan),
        None => engine::ConfigSource::Dir(launch.config_dir.as_deref()),
    };
    match engine::settings(
        &launch.workspace,
        launch.db,
        launch.model,
        config,
        launch.auto,
        launch.agent.as_deref(),
        launch.remote_host.clone(),
    ) {
        Ok(settings) => {
            app.set_settings(settings.clone());
            if let Some(notice) = engine::unrouted_notice(
                &settings.spawn.args,
                &settings.model,
                std::env::var_os("MIAO_ENGINE_REAL_BIN").is_some(),
            ) {
                app.warn(&notice);
            }
            match engine::attach(&settings, bus.clone()) {
                Ok(handle) => app.attach_engine(handle),
                Err(message) => app.handle(AppEvent::EngineFailure(message)),
            }
        }
        Err(message) => app.handle(AppEvent::EngineFailure(message)),
    }
    app.set_bus(bus.clone());
    let input = input::spawn(bus.clone());
    app.start();
    let mut title = terminal_title::Title::new(std::io::stdout());
    // A paint whose PTY cannot accept it must never end the run. A hidden
    // web-IDE tab stops draining the output, so writes block or start failing
    // until the operator comes back; the loop skips those frames and keeps the
    // engine tasks, the session, and the process alive.
    let mut paint_failures = 0u32;
    // ratatui writes only what differs from its previous buffer. A frame the
    // terminal dropped leaves that buffer believing the text is already on
    // screen, so nothing re-sends it and the transcript freezes until an
    // unrelated full repaint (a keystroke, a resize) repairs it. `clear`
    // resets the back buffer so the next draw repaints everything.
    let mut repaint_all = false;
    let mut update_check = tokio::time::interval(update::check_interval());
    update_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let _ = title.update(app.terminal_title().as_deref());
        // A settled scroll asks for the same seamless repaint a failed paint
        // does: the rows it moved may have lost a cell in the terminal's copy,
        // and the diff will not re-send a cell it believes is already drawn.
        if app.take_repaint_all_request() {
            repaint_all = true;
        }
        if repaint_all {
            // A paint failed, so the terminal is missing cells ratatui believes
            // it already wrote. Emptying both diff buffers makes the next draw
            // resend every cell — including the ones that were lost — without
            // touching the visible screen. `Terminal::clear` would send
            // Clear(All), wiping the screen to blank before the repaint lands:
            // under streaming backpressure a write fails often, and each
            // failure flashed the whole transcript (the "constant flicker"
            // report). Repainting identical cells over what is already shown
            // is byte-heavy but visually seamless. Two swaps reset both
            // buffers and leave `current` where it started.
            repaint_all = false;
            terminal.swap_buffers();
            terminal.swap_buffers();
        }
        match crate::terminal::draw_frame(terminal, |frame| app.render(frame)) {
            Ok(_) => paint_failures = 0,
            Err(error) => {
                paint_failures += 1;
                repaint_all = true;
                if paint_failures == 1 {
                    app.warn(&format!(
                        "paint failed ({error}); holding frames until output drains"
                    ));
                }
                if paint_failures > 60 {
                    // A minute of unresponsive output is a dead terminal, not
                    // backpressure; say so instead of spinning.
                    return Err(format!("painting kept failing: {error}").into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        if std::env::var_os("MIAO_TUI_TEST_PANIC").is_some() {
            panic!("miao-tui: injected panic (MIAO_TUI_TEST_PANIC)");
        }
        if app.quit {
            app.report_mtty_quit();
            return Ok(());
        }
        // An active run's status counts elapsed time and reports an engine that
        // stopped answering, so it repaints every second. An idle screen only
        // changes when a relative timestamp would: "2h ago" stays true for
        // another hour, and a screen with no timestamps does not change at all.
        // Waking on that boundary instead of a fixed minute keeps an idle TUI
        // from repainting sixty times an hour to redraw identical text. Input and
        // engine events repaint immediately; clock ticks are never replayed after
        // an editor.
        //
        // A zero-sized frame paints nothing: a pty whose window size was not
        // set by the time the TUI drew its first frame leaves the screen blank,
        // and the loop parks waiting for an event — an operator sees a dead
        // process. A resize repairs it, but a resize may never come. When the
        // frame has no cells, poll for a real size every 200 ms and paint the
        // moment one arrives.
        let sized = terminal
            .size()
            .map(|size| size.width > 0 && size.height > 0)
            .unwrap_or(false);
        let after = if !sized {
            Some(Duration::from_millis(200))
        } else {
            match app.busy() {
                // A running turn repaints at least once a second so its elapsed
                // clock and its engine-silence warning stay true. A pending draft
                // is a reason to come back sooner: the stream writes far more often
                // than once a second, and waiting for the clock tick let the newest
                // text sit unpainted until the next event arrived.
                true => Some(match app.next_repaint() {
                    Some(sooner) => sooner.min(Duration::from_secs(1)),
                    None => Duration::from_secs(1),
                }),
                false => app.next_repaint(),
            }
        };
        let repaint = async {
            match after {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = termination.next() => return Ok(()),
            // The clock woke the loop: the app may owe the engine a read (a
            // delegated child's progress, which arrives on no event of ours).
            _ = repaint => app.tick(),
            _ = update_check.tick() => {
                let bus = bus.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(version) = update::auto_update() {
                        bus.send(AppEvent::UpdateReady(version));
                    }
                });
            }
            event = events.recv() => match event {
                Some(app_event) => {
                    if matches!(&app_event, AppEvent::Focus(true) | AppEvent::Resize { .. }) {
                        title.invalidate();
                    }
                    app.handle(app_event);
                    // Coalesce before painting. A run that streams deltas puts
                    // one event in this queue per token, and a keystroke lands
                    // behind all of them: handling a single event per frame
                    // spends the loop repainting the transcript while the echo
                    // of what the operator typed waits in line. Draining here
                    // means one paint per batch instead of one per token.
                    while let Ok(queued) = events.try_recv() {
                        app.handle(queued);
                    }
                    if app.take_redraw_request() {
                        let _ = terminal::redraw(terminal);
                    }
                    #[cfg(unix)]
                    if app.take_suspend_request() {
                        let Ok(paused) = input.pause() else {
                            app.warn("cannot suspend: the input reader stopped");
                            continue;
                        };
                        let _ = title.update(None);
                        terminal::suspend();
                        let result = platform::job_control::suspend();
                        if let Err(error) = terminal::resume(terminal, preferences) {
                            app.warn(&format!("terminal did not resume cleanly: {error}"));
                        }
                        drop(paused);
                        title.invalidate();
                        if let Err(error) = result {
                            app.warn(&format!("cannot suspend to shell: {error}"));
                        }
                    }
                    if let Some(request) = app.take_editor_request() {
                        let Ok(paused) = input.pause() else {
                            app.warn("cannot open the editor: the input reader stopped");
                            continue;
                        };
                        terminal::suspend();
                        let editor = platform::editor::run(&request);
                        tokio::pin!(editor);
                        let result = loop {
                            tokio::select! {
                                biased;
                                signal = termination.next() => {
                                    // Cooked Ctrl+C also reaches the foreground
                                    // editor. Let it cancel editing, not the UI.
                                    if signal == platform::signals::Signal::Exit {
                                        return Ok(());
                                    }
                                }
                                result = &mut editor => break result,
                            }
                        };
                        if let Err(error) = terminal::resume(terminal, preferences) {
                            app.warn(&format!("terminal did not resume cleanly: {error}"));
                        }
                        drop(paused);
                        app.handle(AppEvent::EditorFinished(result));
                    }
                }
                None => {
                    // Every sender died while the UI is alive, which should
                    // be impossible; say so instead of vanishing without a
                    // trace when a report of a vanished session arrives.
                    eprintln!("miao-tui: every event sender closed; exiting");
                    return Ok(());
                }
            },
        }
    }
}
