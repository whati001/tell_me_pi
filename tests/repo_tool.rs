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
