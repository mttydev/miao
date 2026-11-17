//! The Remote Control host sidecar.
//!
//! `miao serve --remote-host` and the TUI both start the same `miao-remote`
//! executable beside the product binary, handing it a loopback engine endpoint,
//! a one-shot runtime identity, the account registration copied by
//! `miao-remote register`, and the engine database path whose published
//! Sessions it may serve. Keeping the lookup here means a packaged install
//! resolves the sidecar the same way from either entry point.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Start the sidecar for one Runtime.
///
/// The engine token travels in the environment, never argv: the process table
/// is readable by other local users on most systems. `log` names the file that
/// receives the sidecar's own output when the caller owns a TUI screen and
/// cannot inherit a console.
pub fn spawn(
    config: &Path,
    storage: &Path,
    engine_url: &str,
    engine_token: &str,
    runtime_id: &str,
    log: Option<&Path>,
) -> Result<Child, String> {
    if !config.is_file() {
        return Err(format!("--remote-host {} is not a file", config.display()));
    }
    let name = if cfg!(windows) {
        "miao-remote.exe"
    } else {
        "miao-remote"
    };
    let explicit = std::env::var_os("MIAO_REMOTE_HOST_BIN").map(PathBuf::from);
    let script = std::env::var_os("MIAO_REMOTE_HOST_SCRIPT").map(PathBuf::from);
    let binary = explicit.or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|directory| directory.join(name)))
    });
    let (program, args) = match (&binary, &script) {
        (Some(binary), _) if binary.is_file() => (binary.clone(), Vec::new()),
        (_, Some(script)) if script.is_file() => (
            PathBuf::from("bun"),
            vec!["run".to_string(), script.display().to_string()],
        ),
        (Some(binary), _) => {
            return Err(format!(
                "Remote Control needs {} beside miao (or set MIAO_REMOTE_HOST_BIN)",
                binary.display()
            ))
        }
        (None, Some(script)) => {
            return Err(format!(
                "MIAO_REMOTE_HOST_SCRIPT {} is not a file",
                script.display()
            ))
        }
        (None, None) => return Err(
            "cannot locate miao-remote beside miao; build it with `bun script/build-remote.ts` \
                 or set MIAO_REMOTE_HOST_BIN"
                .into(),
        ),
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .env("MIAO_ENGINE_HTTP_URL", engine_url)
        .env("MIAO_ENGINE_HTTP_TOKEN", engine_token)
        .env("MIAO_RUNTIME_ID", runtime_id)
        .env("MIAO_REMOTE_CONTROL_CONFIG", config)
        .env("MIAO_REMOTE_STORAGE", storage)
        .stdin(Stdio::null());
    if let Some(path) = log {
        let file = log_file(path)?;
        let errors = file.try_clone().map_err(|error| error.to_string())?;
        command
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(errors));
    }
    command
        .spawn()
        .map_err(|error| format!("start remote host: {error}"))
}

fn log_file(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_registration_file_is_refused_before_looking_for_the_sidecar() {
        let directory =
            std::env::temp_dir().join(format!("miao-remote-host-{}", std::process::id()));
        let error = spawn(
            &directory.join("missing.json"),
            &directory.join("miao.db"),
            "http://127.0.0.1:1",
            "token",
            "runtime",
            None,
        )
        .expect_err("a missing registration must fail");
        assert!(error.contains("is not a file"), "{error}");
    }
}
