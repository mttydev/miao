//! What the installed launcher points at, when it is newer than this build.
//!
//! The footer shows the running build's version, which is a compile-time
//! constant: an upgrade swaps the launcher's symlink and never touches a process
//! already running. Without this, an operator who upgraded sees the old version
//! and concludes the upgrade did not happen.

use std::path::PathBuf;
use std::time::Duration;

/// The stable launcher the installer repoints. `miao-preview` and `miao-dev`
/// live elsewhere and are their own channels, so a build that is not this file
/// has nothing to report.
fn launcher() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".miao/bin/miao");
    path.exists().then_some(path)
}

/// `miao 0.3.68` reads as `0.3.68`; anything else reads as nothing.
pub fn parse(output: &str) -> Option<String> {
    let version = output.split_whitespace().nth(1)?;
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() < 2
        || !parts
            .iter()
            .all(|part| part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(version.to_string())
}

/// Numeric comparison of dotted versions, so `0.3.9` is older than `0.3.68`.
/// A missing segment counts as zero.
pub fn newer(installed: &str, running: &str) -> bool {
    let segments = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (installed, running) = (segments(installed), segments(running));
    let width = installed.len().max(running.len());
    for index in 0..width {
        let left = installed.get(index).copied().unwrap_or(0);
        let right = running.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// The installed version when it is newer than this build. The launcher is asked
/// for its version directly: the installer leaves no metadata beside the binary,
/// so the only authority is the binary itself.
pub fn pending() -> Option<String> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    let launcher = std::fs::canonicalize(launcher()?).ok()?;
    // Running the installed build already: nothing to announce.
    if current.as_deref() == Some(launcher.as_path()) {
        return None;
    }
    let output = std::process::Command::new(&launcher)
        .arg("--version")
        .output()
        .ok()?;
    let installed = parse(std::str::from_utf8(&output.stdout).ok()?)?;
    newer(&installed, crate::VERSION).then_some(installed)
}

/// Whether this process is the release launcher's own build. `miao-dev` and
/// `miao-preview` are separate channels: they must never repoint the release
/// launcher, and the release launcher is the only build that may update itself.
pub fn is_release_build() -> bool {
    let Some(running) = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
    else {
        return false;
    };
    let Some(installed) = launcher().and_then(|path| std::fs::canonicalize(path).ok()) else {
        return false;
    };
    running == installed
}

/// How often a running release build re-checks. The upstream docs promise
/// "startup and periodically while running" without a number; an hour notices
/// a release within one work session and costs one redirect per check.
pub fn check_interval() -> Duration {
    let seconds = std::env::var("MIAO_UPDATE_CHECK_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds >= 60)
        .unwrap_or(3600);
    Duration::from_secs(seconds)
}

/// The version staged by this check, if any. Installs the newest release in the
/// background, the way `miao upgrade` repoints the launcher, and leaves the
/// running process on its own build until the operator restarts.
pub fn auto_update() -> Option<String> {
    if let Some(installed) = pending() {
        return Some(installed);
    }
    if !is_release_build() {
        return None;
    }
    let _lock = UpdateLock::acquire()?;
    let running = std::env::current_exe().ok()?;
    let _ = std::process::Command::new(running)
        .arg("upgrade")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    pending()
}

/// One updater per installation. A window that loses the race checks again on
/// its next interval instead of installing the same release twice.
struct UpdateLock(PathBuf);

impl UpdateLock {
    fn acquire() -> Option<Self> {
        let path = launcher()?.with_file_name(".update.lock");
        // A process killed mid-install leaves the file behind; age it out so
        // the next check is not suppressed forever.
        if std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(10 * 60))
        {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        Some(Self(path))
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{newer, parse};

    #[test]
    fn a_version_line_parses_and_noise_does_not() {
        assert_eq!(parse("miao 0.3.68\n"), Some("0.3.68".into()));
        assert_eq!(parse("miao"), None);
        assert_eq!(parse("miao next"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn comparison_is_numeric_not_lexical() {
        assert!(newer("0.3.68", "0.3.65"));
        assert!(!newer("0.3.9", "0.3.68"), "9 < 68 numerically");
        assert!(newer("0.4.0", "0.3.99"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.3.68", "0.3.68"));
        assert!(!newer("0.3.65", "0.3.68"));
    }
}
