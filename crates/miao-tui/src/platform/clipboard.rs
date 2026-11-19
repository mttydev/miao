//! Clipboard reads and writes.
//!
//! Writes go through OSC 52 first (which works over SSH, tmux, and in most
//! modern terminals) and then through a platform command. Reads prefer an
//! image because that is what a TUI paste most often carries on macOS and
//! Windows; text is the fallback.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;

/// A paste serves the image the host most recently wrote. The file is not
/// deleted after a read: the host rewrites it on every paste, and the platform
/// fallback inside a sandboxed pane may be refused.
///
/// Identity is the *write* (mtime, length, digest), not the bytes alone: a
/// second screenshot can be byte-identical (the same window twice) and must
/// still attach, while reading the same write twice must not. A pane restored
/// after the host restarted also keeps the handoff path of the dead host, so the
/// read falls back to the newest fresh `mtty-clipboard-*.png` beside it.
static LAST_HANDOFF: std::sync::Mutex<Option<Handoff>> = std::sync::Mutex::new(None);

/// How recently the host must have written a handoff file for a paste to use it.
/// The host writes immediately before sending the empty paste, so this only
/// rejects a file left over from an earlier paste (or another pane).
const HANDOFF_FRESH_MS: u128 = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Handoff {
    modified: Option<std::time::SystemTime>,
    len: u64,
    digest: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Image { mime: String, data: String },
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Environment {
    pub os: &'static str,
    pub wayland: bool,
    pub tmux: bool,
    pub screen: bool,
}

impl Environment {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            screen: std::env::var_os("STY").is_some(),
        }
    }
}

/// Copy `text` everywhere the terminal can carry it. Osc 52 is unconditional
/// when stdout is a terminal; the platform command is best effort.
pub fn copy(text: &str) {
    if io::stdout().is_terminal() {
        let env = Environment::current();
        let mut stdout = io::stdout();
        let _ = stdout.write_all(osc52(text, env.tmux, env.screen).as_bytes());
        let _ = stdout.flush();
    }
    let env = Environment::current();
    if let Some((program, args)) = copy_command(env.os, env.wayland, command_exists) {
        run_with_input(&program, &args, text.as_bytes());
    }
}

/// OSC 52, wrapped for tmux and screen the same way `clipboard.ts` does.
pub fn osc52(text: &str, tmux: bool, screen: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    let passthrough = format!("\x1bPtmux;\x1b{sequence}\x1b\\");
    if tmux {
        format!("{sequence}{passthrough}")
    } else if screen {
        passthrough
    } else {
        sequence
    }
}

/// The first platform command able to take text on stdin.
pub fn copy_command(
    os: &str,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<(String, Vec<String>)> {
    match os {
        "macos" if has("pbcopy") => Some(("pbcopy".into(), Vec::new())),
        "linux" if wayland && has("wl-copy") => Some(("wl-copy".into(), Vec::new())),
        "linux" if has("xclip") => Some((
            "xclip".into(),
            vec!["-selection".into(), "clipboard".into()],
        )),
        "linux" if has("xsel") => {
            Some(("xsel".into(), vec!["--clipboard".into(), "--input".into()]))
        }
        "windows" if has("powershell.exe") => Some((
            "powershell.exe".into(),
            vec![
                "-NonInteractive".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Set-Clipboard -Value ([Console]::In.ReadToEnd())".into(),
            ],
        )),
        _ => None,
    }
}

/// Read the clipboard, preferring an image. `host_file` is the mtty handoff
/// path (`MTTY_CLIPBOARD_FILE`); each picture is served once.
pub fn read(host_file: Option<&Path>) -> Option<Content> {
    if let Some(content) = read_host_image(host_file) {
        return Some(content);
    }
    let env = Environment::current();
    match env.os {
        "macos" => read_macos().or_else(read_text_macos),
        "windows" => read_windows().or_else(read_text_windows),
        "linux" => read_linux(env.wayland).or_else(|| read_text_linux(env.wayland)),
        _ => None,
    }
}

fn read_host_image(path: Option<&Path>) -> Option<Content> {
    let path = fresh_handoff(path?)?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    // A concurrent write can truncate the file between the read and the
    // metadata call. The next paste carries that write; this one cannot claim
    // a half picture.
    if metadata.len() != bytes.len() as u64 {
        return None;
    }
    // One paste, one attachment: a repeated empty paste (a terminal without
    // image support, a duplicate event) must not attach the same write twice.
    // The same picture pasted again is a new write with a new mtime, so it is
    // still served.
    let identity = Handoff {
        modified: metadata.modified().ok(),
        len: bytes.len() as u64,
        digest: digest(&bytes),
    };
    {
        let mut last = LAST_HANDOFF.lock().expect("handoff lock");
        if *last == Some(identity) {
            return None;
        }
        *last = Some(identity);
    }
    Some(Content::Image {
        mime: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn digest(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// The handoff file this paste wrote.
///
/// `configured` is the path the pane inherited. A restored pane keeps the
/// environment of the mtty process that created it, so after a host restart it
/// still names the dead host's file while the live host writes a sibling
/// `mtty-clipboard-<pid>.png`. The newest fresh sibling is the one this paste
/// wrote.
///
/// Only a fresh file is a paste. An old file is an earlier paste, and serving
/// it again would attach yesterday's screenshot to today's prompt; the host
/// writes immediately before sending the empty paste, so ten seconds is
/// generous even for a large Retina capture.
fn fresh_handoff(configured: &Path) -> Option<std::path::PathBuf> {
    let fresh = |path: &Path| {
        std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .elapsed()
            .ok()
            .filter(|age| age.as_millis() <= HANDOFF_FRESH_MS)
            .map(|_| path.to_path_buf())
    };
    if let Some(path) = fresh(configured) {
        return Some(path);
    }
    let dir = configured.parent()?;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("mtty-clipboard-") && name.ends_with(".png"))
        })
        .filter_map(|entry| fresh(&entry.path()))
        .max_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
}

fn read_macos() -> Option<Content> {
    let file = std::env::temp_dir().join(format!(
        "miao-clipboard-{}-{}.png",
        std::process::id(),
        unique_suffix()
    ));
    let script = format!(
        r#"ObjC.import("AppKit")
const pasteboard = $.NSPasteboard.generalPasteboard
let image = pasteboard.dataForType("public.png")
if (image.isNil()) {{
  const tiff = pasteboard.dataForType("public.tiff")
  if (tiff.isNil()) throw new Error("clipboard holds no image")
  image = $.NSBitmapImageRep.imageRepWithData(tiff).representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $())
}}
if (image.isNil()) throw new Error("could not encode the clipboard image")
image.writeToFileAtomically({:?}, true)"#,
        file.to_string_lossy()
    );
    let status = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let bytes = std::fs::read(&file).ok();
    if !status.is_ok_and(|status| status.success()) {
        let _ = std::fs::remove_file(&file);
        return None;
    }
    let mut bytes = bytes.filter(|bytes| !bytes.is_empty())?;
    let mut mime = "image/png".to_string();
    // A Retina screenshot dwarfs the engine's 8 MiB base64 attachment cap, so
    // the admit would bounce after the operator already pasted it (issue
    // #2959: the prompt sends, the model never sees the picture). sips ships
    // with macOS: re-encode the oversized ones as JPEG at a bounded size.
    if bytes.len() > 4 * 1024 * 1024 {
        let jpeg = file.with_extension("jpg");
        let encoded = Command::new("sips")
            .args([
                "-s",
                "format",
                "jpeg",
                "-s",
                "formatOptions",
                "80",
                "--resampleHeightWidthMax",
                "2400",
            ])
            .arg(&file)
            .arg(&jpeg)
            .output()
            .is_ok_and(|output| output.status.success());
        if encoded {
            if let Ok(smaller) = std::fs::read(&jpeg) {
                if !smaller.is_empty() && smaller.len() < bytes.len() {
                    bytes = smaller;
                    mime = "image/jpeg".into();
                }
            }
        }
        let _ = std::fs::remove_file(&jpeg);
    }
    let _ = std::fs::remove_file(&file);
    Some(Content::Image {
        mime,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn read_text_macos() -> Option<Content> {
    let output = Command::new("pbpaste").stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    non_empty_text(text)
}

/// The Windows image bridge: the encoding on the first line, the image as
/// base64 on the second.
///
/// An oversized screenshot is re-encoded as JPEG here, the way `read_macos`
/// uses `sips`: a 4K PNG's base64 can exceed the engine's 8 MiB attachment
/// bound, so the prompt would bounce after the operator already pasted it.
/// A file copied in Explorer carries no bitmap, so the file-drop list is the
/// second source.
const WINDOWS_IMAGE_SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img -eq $null) { foreach ($path in [System.Windows.Forms.Clipboard]::GetFileDropList()) { try { $loaded = [System.Drawing.Image]::FromFile($path); if ($loaded -ne $null) { $img = $loaded; break } } catch {} } }; if ($img -ne $null) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); $bytes = $ms.ToArray(); $mime = 'image/png'; if ($bytes.Length -gt 4194304) { $scale = [Math]::Min(1.0, 2400.0 / [Math]::Max($img.Width, $img.Height)); $width = [int][Math]::Max(1, $img.Width * $scale); $height = [int][Math]::Max(1, $img.Height * $scale); $bmp = New-Object System.Drawing.Bitmap -ArgumentList $width, $height; $g = [System.Drawing.Graphics]::FromImage($bmp); $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic; $g.DrawImage($img, 0, 0, $width, $height); $g.Dispose(); $jpg = New-Object System.IO.MemoryStream; $bmp.Save($jpg, [System.Drawing.Imaging.ImageFormat]::Jpeg); if ($jpg.Length -lt $bytes.Length) { $bytes = $jpg.ToArray(); $mime = 'image/jpeg' }; $jpg.Dispose(); $bmp.Dispose() }; $ms.Dispose(); $img.Dispose(); Write-Output $mime; Write-Output ([System.Convert]::ToBase64String($bytes)) }"#;

/// Windows clipboard access is contended: the app that owns the clipboard may
/// hold it while another read races it, and `OpenClipboard` fails with
/// `CLIPBRD_E_CANT_OPEN`. A few short retries absorb that race; the count is
/// bounded so a wedged clipboard costs a moment, not a hang.
const WINDOWS_READ_ATTEMPTS: u32 = 3;
const WINDOWS_RETRY_DELAY_MS: u64 = 50;
/// A wedged clipboard owner can hold `OpenClipboard` for tens of seconds. The
/// read runs off the UI thread, but a stuck script would keep the paste flag
/// set for the rest of the session, so each attempt is bounded and killed.
/// Ten seconds leaves room for a cold `Add-Type` plus the 2400px re-encode of a
/// large screenshot, which together can outlast a tight five-second bound.
const WINDOWS_SCRIPT_TIMEOUT_MS: u64 = 10_000;
/// How long the drain waits after the script settled or was killed. A process
/// killed on timeout can leave a grandchild holding stdout; joining the reader
/// forever would keep the paste flag set for the rest of the session, which is
/// exactly the silence this path exists to end.
const WINDOWS_DRAIN_GRACE_MS: u64 = 2_000;

/// One attempt to run a Windows clipboard script. `Output` carries the stdout
/// bytes of a clean exit; `Failed` covers spawn errors and nonzero exits, the
/// transient cases a retry can still recover.
enum WindowsOutput {
    Output(Vec<u8>),
    Failed,
}

/// Read the clipboard through a script, retrying script failures. A script
/// that ran and found nothing is a definitive answer and stops the retries;
/// `parse` decides what the bytes mean.
fn windows_read<T>(
    script: &str,
    delay_ms: u64,
    mut invoke: impl FnMut(&str) -> WindowsOutput,
    parse: impl Fn(&[u8]) -> Option<T>,
) -> Option<T> {
    for attempt in 1..=WINDOWS_READ_ATTEMPTS {
        match invoke(script) {
            WindowsOutput::Output(bytes) => return parse(&bytes),
            WindowsOutput::Failed if attempt < WINDOWS_READ_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            WindowsOutput::Failed => return None,
        }
    }
    None
}

/// The PowerShell that will run the clipboard scripts.
///
/// `powershell.exe` is not always on `PATH` (a packaged launcher can replace
/// it), so the System32 copy is preferred. `pwsh.exe` covers a machine where
/// Windows PowerShell was removed or is not installed.
fn windows_shell() -> Option<PathBuf> {
    let system = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
        .filter(|path| path.is_file());
    if system.is_some() {
        return system;
    }
    ["powershell.exe", "pwsh.exe"]
        .into_iter()
        .find(|name| command_exists(name))
        .map(PathBuf::from)
}

fn run_powershell(script: &str) -> WindowsOutput {
    let Some(shell) = windows_shell() else {
        return WindowsOutput::Failed;
    };
    let Ok(mut child) = Command::new(&shell)
        .args(["-NonInteractive", "-NoProfile", "-STA", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return WindowsOutput::Failed;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return WindowsOutput::Failed;
    };
    // A base64 payload can exceed a pipe buffer, so it is drained while the
    // child runs; waiting first would deadlock once the buffer filled. The
    // reader reports over a channel so a pipe a grandchild keeps open times out
    // instead of pinning the caller.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = std::io::BufReader::new(stdout)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(read);
    });
    let deadline = Instant::now() + Duration::from_millis(WINDOWS_SCRIPT_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = rx
        .recv_timeout(Duration::from_millis(WINDOWS_DRAIN_GRACE_MS))
        .ok()
        .and_then(|read| read.ok());
    match (status, bytes) {
        (Some(status), Some(bytes)) if status.success() => WindowsOutput::Output(bytes),
        _ => WindowsOutput::Failed,
    }
}

fn read_windows() -> Option<Content> {
    windows_read(
        WINDOWS_IMAGE_SCRIPT,
        WINDOWS_RETRY_DELAY_MS,
        run_powershell,
        parse_windows_image,
    )
}

fn read_text_windows() -> Option<Content> {
    windows_read(
        "Get-Clipboard -Raw",
        WINDOWS_RETRY_DELAY_MS,
        run_powershell,
        parse_windows_text,
    )
}

fn parse_windows_image(bytes: &[u8]) -> Option<Content> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let (mime, payload) = match text.split_once('\n') {
        Some((header, payload)) => {
            let header = header.trim_end_matches('\r');
            if header != "image/png" && header != "image/jpeg" {
                return None;
            }
            (header, payload.trim())
        }
        None => ("image/png", text),
    };
    let data = canonical_base64(payload)?;
    Some(Content::Image {
        mime: mime.into(),
        data,
    })
}

/// Keep only canonical base64, so console noise can never reach the engine as
/// pixels.
fn canonical_base64(text: &str) -> Option<String> {
    let canonical = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
    canonical.then(|| text.to_string())
}

/// `-Raw` output carries one console-added newline terminator; a newline the
/// clipboard itself holds survives as the second one, so trim exactly one.
fn parse_windows_text(bytes: &[u8]) -> Option<Content> {
    non_empty_text(trim_console_newline(
        String::from_utf8_lossy(bytes).to_string(),
    ))
}

fn trim_console_newline(text: String) -> String {
    text.strip_suffix("\r\n")
        .map(str::to_string)
        .unwrap_or_else(|| text.strip_suffix('\n').map(str::to_string).unwrap_or(text))
}

fn read_linux(wayland: bool) -> Option<Content> {
    let mut candidates: Vec<(&str, Vec<&str>)> = Vec::new();
    if wayland {
        candidates.push(("wl-paste", vec!["-t", "image/png"]));
    }
    candidates.push((
        "xclip",
        vec!["-selection", "clipboard", "-t", "image/png", "-o"],
    ));
    for (program, args) in candidates {
        let Some(output) = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .ok()
        else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            return Some(Content::Image {
                mime: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(output.stdout),
            });
        }
    }
    None
}

fn read_text_linux(wayland: bool) -> Option<Content> {
    let mut candidates: Vec<(&str, Vec<&str>)> = Vec::new();
    if wayland {
        candidates.push(("wl-paste", vec!["--no-newline"]));
    }
    candidates.push(("xclip", vec!["-selection", "clipboard", "-o"]));
    candidates.push(("xsel", vec!["--clipboard", "--output"]));
    for (program, args) in candidates {
        let Some(output) = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .ok()
        else {
            continue;
        };
        if output.status.success() {
            if let Some(content) =
                non_empty_text(String::from_utf8_lossy(&output.stdout).to_string())
            {
                return Some(content);
            }
        }
    }
    None
}

fn non_empty_text(text: String) -> Option<Content> {
    if text.is_empty() {
        return None;
    }
    Some(Content::Text(text))
}

fn run_with_input(program: &str, args: &[String], input: &[u8]) {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let _ = child.wait();
}

/// PATH lookup without pulling in a crate. Windows command names are matched
/// case-insensitively and with PATHEXT extensions.
pub fn command_exists(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let names = if cfg!(windows) {
        let extensions = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut names = vec![name.to_string()];
        names.extend(
            extensions
                .into_iter()
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{name}{extension}")),
        );
        names
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&paths).any(|dir| {
        names.iter().any(|name| {
            let candidate = dir.join(name);
            // One expression per platform: the early return followed by a bare
            // `true` reads as a boolean literal to clippy >= 1.100, which then
            // fails `-D warnings` on the Windows leg only.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                candidate.is_file()
                    && candidate
                        .metadata()
                        .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
    })
}

fn unique_suffix() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COPIES: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    (nanos << 8) | u128::from(COPIES.fetch_add(1, Ordering::Relaxed) % 256)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LAST_HANDOFF` and the temp directory are process-wide; serialise the
    /// handoff tests so one test's write cannot be served by another.
    fn handoff_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *LAST_HANDOFF.lock().expect("handoff lock") = None;
        guard
    }

    /// Write a picture and give it an mtime distinct from the previous write,
    /// even on a coarse filesystem clock. The host's write is what a paste is;
    /// the bytes alone are not, because the same screen can be captured twice.
    fn rewrite(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_millis(100))
            .unwrap();
    }

    fn handoff_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miao-clipboard-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn has(name: &'static str) -> impl Fn(&str) -> bool {
        move |candidate| candidate == name
    }

    #[test]
    fn osc52_wraps_for_tmux_and_screen() {
        assert_eq!(osc52("hi", false, false), "\x1b]52;c;aGk=\x07");
        let screen = osc52("hi", false, true);
        assert_eq!(screen, "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        let tmux = osc52("hi", true, false);
        assert!(tmux.starts_with("\x1b]52;c;aGk=\x07\x1bPtmux;"));
    }

    #[test]
    fn copy_prefers_platform_native_commands_in_the_typescript_order() {
        assert_eq!(
            copy_command("macos", false, has("pbcopy")).map(|item| item.0),
            Some("pbcopy".into())
        );
        assert_eq!(
            copy_command("linux", true, has("xclip")).map(|item| item.0),
            Some("xclip".into())
        );
        assert_eq!(
            copy_command("linux", true, |name| name == "wl-copy").map(|item| item.0),
            Some("wl-copy".into())
        );
        assert_eq!(
            copy_command("linux", false, |_| true).map(|item| item.0),
            Some("xclip".into())
        );
        assert_eq!(copy_command("freebsd", false, |_| true), None);
    }

    /// One picture, one attachment — and the file stays where the host can
    /// rewrite it. Deleting it on read made the *second* screenshot depend on
    /// `osascript`, which a sandboxed pane may refuse, so the paste silently did
    /// nothing.
    #[test]
    fn the_host_clipboard_file_is_served_once_per_write() {
        let _guard = handoff_guard();
        let dir = handoff_dir("served-once");
        let fresh = dir.join("mtty-clipboard-5554.png");
        rewrite(&fresh, b"first-picture");
        let content = read_host_image(Some(&fresh)).expect("fresh image");
        assert_eq!(
            content,
            Content::Image {
                mime: "image/png".into(),
                data: "Zmlyc3QtcGljdHVyZQ==".into()
            }
        );
        assert!(fresh.exists(), "the host keeps the file it will rewrite");

        // Reading the same write again (a duplicate empty paste) does not
        // attach a second copy.
        assert!(read_host_image(Some(&fresh)).is_none());

        // The same picture pasted again is a new write with a new mtime. The
        // bytes are identical, so content alone cannot tell it apart — and the
        // operator did paste it again.
        rewrite(&fresh, b"first-picture");
        let content = read_host_image(Some(&fresh)).expect("the same picture, pasted again");
        assert_eq!(
            content,
            Content::Image {
                mime: "image/png".into(),
                data: "Zmlyc3QtcGljdHVyZQ==".into()
            }
        );

        // A different picture is served for its own write too.
        rewrite(&fresh, b"second-picture");
        let content = read_host_image(Some(&fresh)).expect("the second picture");
        assert_eq!(
            content,
            Content::Image {
                mime: "image/png".into(),
                data: "c2Vjb25kLXBpY3R1cmU=".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pane restored after the host restarted keeps the dead host's path in
    /// `MTTY_CLIPBOARD_FILE`. The live host writes a sibling with its own pid;
    /// that is the file the paste just produced.
    #[test]
    fn a_restored_pane_reads_the_live_hosts_handoff() {
        let _guard = handoff_guard();
        let dir = handoff_dir("restored");
        let dead = dir.join("mtty-clipboard-1504.png");
        let live = dir.join("mtty-clipboard-5554.png");
        rewrite(&live, b"live-picture");

        let content = read_host_image(Some(&dead)).expect("the live host's file");
        assert_eq!(
            content,
            Content::Image {
                mime: "image/png".into(),
                data: "bGl2ZS1waWN0dXJl".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An old handoff is an earlier paste, not this one. Serving it again would
    /// attach yesterday's screenshot to today's prompt.
    #[test]
    fn an_old_handoff_is_not_todays_paste() {
        let _guard = handoff_guard();
        let dir = handoff_dir("old");
        let dead = dir.join("mtty-clipboard-1504.png");
        let old = dir.join("mtty-clipboard-5554.png");
        std::fs::write(&old, b"old-picture").unwrap();
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(30))
            .unwrap();
        assert!(read_host_image(Some(&dead)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_handoff_file_is_not_an_image() {
        let _guard = handoff_guard();
        let dir = handoff_dir("empty");
        let path = dir.join("mtty-clipboard-5554.png");
        std::fs::write(&path, b"").unwrap();
        assert!(read_host_image(Some(&path)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Windows retry harness: a contended clipboard can fail to open a few
    /// times before it answers. `windows_read` takes its script runner as a
    /// parameter, so the retry semantics run on every platform's CI, not only
    /// where a clipboard exists.
    #[test]
    fn windows_read_retries_a_failed_script_until_it_runs() {
        let mut attempts = 0;
        let content = windows_read(
            "script",
            0,
            |_| {
                attempts += 1;
                if attempts < 3 {
                    WindowsOutput::Failed
                } else {
                    WindowsOutput::Output(b"cG5n".to_vec())
                }
            },
            parse_windows_image,
        );
        assert_eq!(attempts, 3, "the third attempt carried the answer");
        assert_eq!(
            content,
            Some(Content::Image {
                mime: "image/png".into(),
                data: "cG5n".into()
            })
        );
    }

    /// A script that ran and found nothing is a definitive answer: retrying
    /// cannot turn "no image" into one, so the loop stops after one attempt.
    #[test]
    fn windows_read_stops_at_a_definitive_answer() {
        let mut attempts = 0;
        let content = windows_read(
            "script",
            0,
            |_| {
                attempts += 1;
                WindowsOutput::Output(Vec::new())
            },
            parse_windows_image,
        );
        assert_eq!(attempts, 1, "an empty stdout is an answer, not a failure");
        assert_eq!(content, None);
    }

    /// A wedged clipboard must not hang the read: the attempts are bounded.
    #[test]
    fn windows_read_gives_up_after_the_bounded_attempts() {
        let mut attempts = 0;
        let content = windows_read(
            "script",
            0,
            |_| {
                attempts += 1;
                WindowsOutput::Failed
            },
            parse_windows_image,
        );
        assert_eq!(attempts, WINDOWS_READ_ATTEMPTS);
        assert_eq!(content, None);
    }

    /// The bridge names the encoding it chose (a re-encoded screenshot is
    /// JPEG), and a payload without a header keeps the legacy PNG shape.
    /// Console noise can never reach the engine as pixels.
    #[test]
    fn parse_windows_image_reads_the_encoding_header() {
        assert_eq!(
            parse_windows_image(b"image/png\ncG5n\r\n"),
            Some(Content::Image {
                mime: "image/png".into(),
                data: "cG5n".into()
            })
        );
        assert_eq!(
            parse_windows_image(b"image/jpeg\ncG5n\n"),
            Some(Content::Image {
                mime: "image/jpeg".into(),
                data: "cG5n".into()
            })
        );
        assert_eq!(
            parse_windows_image(b"cG5n\n"),
            Some(Content::Image {
                mime: "image/png".into(),
                data: "cG5n".into()
            })
        );
        assert_eq!(parse_windows_image(b"image/gif\ncG5n\n"), None);
        assert_eq!(parse_windows_image(b"Add-Type : error at line 1"), None);
        assert_eq!(parse_windows_image(b""), None);
    }

    /// Without Windows PowerShell and without `pwsh`, the bridge reports a
    /// failed attempt instead of hanging or flashing a shell window. This is
    /// the shape every non-Windows CI leg sees.
    #[cfg(not(windows))]
    #[test]
    fn a_host_without_powershell_has_no_windows_shell() {
        assert!(windows_shell().is_none());
        assert!(matches!(run_powershell("script"), WindowsOutput::Failed));
    }

    /// PowerShell's `-Raw` adds one newline as a console terminator; a newline
    /// the clipboard itself holds survives as the second one.
    #[test]
    fn parse_windows_text_trims_only_the_console_newline() {
        assert_eq!(
            parse_windows_text(b"hello\r\n"),
            Some(Content::Text("hello".into()))
        );
        assert_eq!(
            parse_windows_text(b"hello\n"),
            Some(Content::Text("hello".into()))
        );
        assert_eq!(
            parse_windows_text(b"a\r\nb\r\n"),
            Some(Content::Text("a\r\nb".into()))
        );
        assert_eq!(
            parse_windows_text(b"\r\n"),
            None,
            "a bare console terminator is not text"
        );
    }
}
