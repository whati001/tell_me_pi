use std::{path::Path, process::Command};

use serde_json::json;
use tell_me_where::{config::RepoConfig, repo::RepoTool};
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Origin repo with `app.txt` = "one" at v1.0.0 and "two" at v1.1.0 (and on main).
fn make_origin(root: &Path) -> String {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q"]);
    git(&origin, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(origin.join("app.txt"), "one").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-qm", "one"]);
    git(&origin, &["tag", "v1.0.0"]);
    std::fs::write(origin.join("app.txt"), "two").unwrap();
    git(&origin, &["commit", "-qam", "two"]);
    git(&origin, &["tag", "v1.1.0"]);
    format!("file://{}", origin.display())
}

fn tool(root: &Path, url: String) -> RepoTool {
    let repos = vec![RepoConfig { name: "app".into(), url, description: "Demo app".into() }];
    RepoTool::new(repos, root.join("mirrors"), None)
}

#[tokio::test]
async fn list_refs_and_checkout() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    let list = tool.execute(&work, json!({"action": "list"}), &progress, &cancel).await.unwrap();
    assert_eq!(list, "- app: Demo app");

    let refs = tool.execute(&work, json!({"action": "refs", "repo": "app"}), &progress, &cancel).await.unwrap();
    let lines: Vec<&str> = refs.lines().collect();
    assert_eq!(&lines[..2], ["tag v1.1.0", "tag v1.0.0"], "{refs}");
    assert!(lines.contains(&"branch main"), "{refs}");

    let out = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.0.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert!(out.contains("./app@v1.0.0/"), "{out}");
    assert_eq!(std::fs::read_to_string(work.join("app@v1.0.0/app.txt")).unwrap(), "one");

    tool.execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.1.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(work.join("app@v1.1.0/app.txt")).unwrap(), "two");

    let again = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v1.0.0"}), &progress, &cancel)
        .await
        .unwrap();
    assert!(again.contains("already checked out"), "{again}");
}

#[tokio::test]
async fn rejects_bad_input() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    for args in [
        json!({"action": "push", "repo": "app"}),
        json!({"action": "checkout", "repo": "other", "ref": "v1.0.0"}),
        json!({"action": "checkout", "repo": "app", "ref": "--upload-pack=touch /tmp/pwned"}),
        json!({"action": "checkout", "repo": "app", "ref": "v9.9.9"}),
    ] {
        let err = tool.execute(&work, args.clone(), &progress, &cancel).await.unwrap_err();
        println!("{args} -> {err:#}");
    }
    let err = tool
        .execute(&work, json!({"action": "checkout", "repo": "app", "ref": "v9.9.9"}), &progress, &cancel)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("action=refs"), "{err}");
}

fn checkout_args(r: &str) -> serde_json::Value {
    json!({"action": "checkout", "repo": "app", "ref": r})
}

#[tokio::test]
async fn refs_with_slash_and_underscore_get_distinct_dirs() {
    let root = tempfile::tempdir().unwrap();
    let url = make_origin(root.path());
    let origin = root.path().join("origin");
    git(&origin, &["branch", "release/1.x", "v1.0.0"]);
    git(&origin, &["branch", "release_1.x", "v1.1.0"]);
    let tool = tool(root.path(), url);
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    let a = tool.execute(&work, checkout_args("release/1.x"), &progress, &cancel).await.unwrap();
    let b = tool.execute(&work, checkout_args("release_1.x"), &progress, &cancel).await.unwrap();
    assert!(a.contains("./app@release_2f1.x/"), "{a}");
    assert!(b.contains("./app@release_5f1.x/"), "{b}");
    assert_eq!(std::fs::read_to_string(work.join("app@release_2f1.x/app.txt")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(work.join("app@release_5f1.x/app.txt")).unwrap(), "two");

    let again = tool.execute(&work, checkout_args("release/1.x"), &progress, &cancel).await.unwrap();
    assert!(again.contains("already checked out"), "{again}");
}

#[tokio::test]
async fn checkout_again_after_workdir_was_deleted() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    tool.execute(&work, checkout_args("v1.0.0"), &progress, &cancel).await.unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let out = tool.execute(&work, checkout_args("v1.0.0"), &progress, &cancel).await.unwrap();
    assert!(out.contains("Checked out"), "{out}");
    assert_eq!(std::fs::read_to_string(work.join("app@v1.0.0/app.txt")).unwrap(), "one");
}

#[tokio::test]
async fn leftover_dir_without_marker_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    let half = work.join("app@v1.0.0");
    std::fs::create_dir_all(half.join(".git")).unwrap();
    std::fs::write(half.join("junk.txt"), "x").unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    let out = tool.execute(&work, checkout_args("v1.0.0"), &progress, &cancel).await.unwrap();
    assert!(out.contains("Checked out"), "{out}");
    assert_eq!(std::fs::read_to_string(half.join("app.txt")).unwrap(), "one");
    assert!(!half.join("junk.txt").exists());
    assert!(work.join(".app@v1.0.0.done").exists());
}

#[tokio::test]
async fn unknown_repo_is_rejected_before_touching_the_filesystem() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    // The work dir does not exist: any filesystem access would produce a different error.
    let work = root.path().join("missing");
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};
    for repo in ["../x", "other"] {
        let args = json!({"action": "checkout", "repo": repo, "ref": "v1.0.0"});
        let err = tool.execute(&work, args, &progress, &cancel).await.unwrap_err();
        assert!(err.to_string().contains("action=list"), "{err}");
    }
    assert!(!root.path().join("mirrors").exists());
    assert!(!root.path().join("x@v1.0.0").exists());
}

#[tokio::test]
async fn fetch_failure_uses_cached_mirror_with_warning() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};
    tool.execute(&work, json!({"action": "refs", "repo": "app"}), &progress, &cancel).await.unwrap();

    // Break the origin and force a refetch by using a fresh tool on the same mirror.
    std::fs::rename(root.path().join("origin"), root.path().join("gone")).unwrap();
    let tool = self::tool(root.path(), format!("file://{}", root.path().join("origin").display()));
    let messages = std::sync::Mutex::new(Vec::new());
    let progress = |m: &str| messages.lock().unwrap().push(m.to_string());
    let refs = tool.execute(&work, json!({"action": "refs", "repo": "app"}), &progress, &cancel).await.unwrap();
    assert!(refs.starts_with("Warning: could not update app ("), "{refs}");
    assert!(refs.contains("tag v1.1.0"), "{refs}");
    assert!(messages.lock().unwrap().iter().any(|m| m.starts_with("fetch failed, using cached mirror")));
}

#[tokio::test]
async fn git_env_of_readable_git_is_not_private() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    assert!(!tool.git_env_is_private().await.unwrap());
}

#[tokio::test]
async fn git_env_of_execute_only_git_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let system_git =
        String::from_utf8(Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let copy = root.path().join("git");
    std::fs::copy(std::fs::canonicalize(system_git.trim()).unwrap(), &copy).unwrap();
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o111)).unwrap();
    let tool = tool(root.path(), make_origin(root.path())).with_git_binary(copy);
    assert!(tool.git_env_is_private().await.unwrap());
}

#[tokio::test]
async fn checkout_recovers_from_missing_but_locked_worktree() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool(root.path(), make_origin(root.path()));
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cancel = CancellationToken::new();
    let progress = |_: &str| {};

    tool.execute(&work, checkout_args("v1.0.0"), &progress, &cancel).await.unwrap();
    // What a killed `git worktree add` leaves behind: a locked worktree entry whose directory is gone.
    let target = work.join("app@v1.0.0");
    let mirror = root.path().join("mirrors/app.git");
    git(root.path(), &["--git-dir", mirror.to_str().unwrap(), "worktree", "lock", target.to_str().unwrap()]);
    std::fs::remove_dir_all(&target).unwrap();
    std::fs::remove_file(work.join(".app@v1.0.0.done")).unwrap();

    let out = tool.execute(&work, checkout_args("v1.0.0"), &progress, &cancel).await.unwrap();
    assert!(out.contains("Checked out"), "{out}");
    assert_eq!(std::fs::read_to_string(target.join("app.txt")).unwrap(), "one");
}
