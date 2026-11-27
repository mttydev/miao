//! `main_checkout` must not start git for roots that cannot be linked
//! worktrees: a long-lived workspace has hundreds of those rows, and two
//! `git rev-parse` processes each is most of the placement-restore walk.

#[cfg(unix)]
#[test]
fn a_checkout_shaped_root_costs_no_git_process() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("checkout");
    std::fs::create_dir_all(root.join(".git")).expect("checkout");

    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).expect("shim dir");
    let marker = dir.path().join("git-ran");
    let shim = bin.join("git");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .expect("shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("mode");

    let previous = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&previous));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("path"));
    let answer = miao_engine::worktree::main_checkout(&root);
    std::env::set_var("PATH", previous);

    assert_eq!(answer, None);
    assert!(
        !marker.exists(),
        "a checkout-shaped root must not start git"
    );
}
