use super::fallback::fallback_ignored_paths;
use super::*;

fn manifest(extra: &str) -> Manifest {
    Manifest::parse(&format!(
        r#"[package]
org = "acme"
name = "pack-inputs"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/pack-inputs.git"

{extra}
"#
    ))
    .unwrap()
}

#[cfg(unix)]
fn git(project: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Pack Inputs")
        .env("GIT_AUTHOR_EMAIL", "zed-pack-inputs@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Pack Inputs")
        .env("GIT_COMMITTER_EMAIL", "zed-pack-inputs@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn git_query_trusts_only_the_canonical_project_path() {
    let project = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(project.path()).unwrap();
    let command = git_ignored_command(project.path()).unwrap();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(args.first().map(String::as_str), Some("-c"));
    assert_eq!(
        args.get(1),
        Some(&format!("safe.directory={}", canonical.display()))
    );
    assert_eq!(args.get(2).map(String::as_str), Some("-C"));
    assert_eq!(args.get(3), Some(&canonical.to_string_lossy().into_owned()));
    assert!(!args.iter().any(|arg| arg == "safe.directory=*"));
}

#[test]
fn git_query_trusts_the_owning_worktree_for_a_nested_package() {
    let worktree = tempfile::tempdir().unwrap();
    fs::create_dir_all(worktree.path().join(".git")).unwrap();
    let project = worktree.path().join("packages/client");
    fs::create_dir_all(&project).unwrap();

    let canonical_worktree = fs::canonicalize(worktree.path()).unwrap();
    let canonical_project = fs::canonicalize(&project).unwrap();
    let command = git_ignored_command(&project).unwrap();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        args.get(1),
        Some(&format!("safe.directory={}", canonical_worktree.display()))
    );
    assert_eq!(
        args.get(3),
        Some(&canonical_project.to_string_lossy().into_owned())
    );
    assert!(!args.iter().any(|arg| arg == "safe.directory=*"));
}

#[test]
fn gitless_fallback_honors_nested_rules_and_negation() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("nested/cache")).unwrap();
    fs::write(project.path().join(".gitignore"), "*.env\n!important.env\n").unwrap();
    fs::write(
        project.path().join("nested/.gitignore"),
        "cache/\nvisible.tmp\n",
    )
    .unwrap();
    fs::write(project.path().join("secret.env"), "secret\n").unwrap();
    fs::write(project.path().join("important.env"), "tracked intent\n").unwrap();
    fs::write(project.path().join("nested/cache/value.txt"), "cache\n").unwrap();
    fs::write(project.path().join("nested/visible.tmp"), "ignored\n").unwrap();

    let paths = fallback_ignored_paths(project.path()).unwrap();
    assert!(paths.contains(&PathBuf::from("secret.env")));
    assert!(paths.contains(&PathBuf::from("nested/cache/value.txt")));
    assert!(paths.contains(&PathBuf::from("nested/visible.tmp")));
    assert!(!paths.contains(&PathBuf::from("important.env")));
}

#[test]
fn gitless_fallback_does_not_reinclude_a_child_of_ignored_parent() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("cache")).unwrap();
    fs::write(
        project.path().join(".gitignore"),
        "cache/\n!cache/value.txt\n",
    )
    .unwrap();
    fs::write(project.path().join("cache/value.txt"), "still ignored\n").unwrap();

    let paths = fallback_ignored_paths(project.path()).unwrap();
    assert_eq!(paths, vec![PathBuf::from("cache/value.txt")]);
}

#[test]
fn gitless_fallback_honors_worktree_rules_above_a_nested_package() {
    let worktree = tempfile::tempdir().unwrap();
    fs::create_dir_all(worktree.path().join(".git/info")).unwrap();
    let project = worktree.path().join("packages/client");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        worktree.path().join(".gitignore"),
        "packages/client/secret.env\n",
    )
    .unwrap();
    fs::write(project.join("secret.env"), "secret\n").unwrap();
    fs::write(project.join("payload.txt"), "safe\n").unwrap();

    let paths = fallback_ignored_paths(&project).unwrap();
    assert_eq!(paths, vec![PathBuf::from("secret.env")]);
}

#[test]
fn gitless_fallback_reads_linked_worktree_common_excludes() {
    let root = tempfile::tempdir().unwrap();
    let worktree = root.path().join("worktree");
    let git_dir = root.path().join("meta/worktrees/client");
    fs::create_dir_all(&worktree).unwrap();
    fs::create_dir_all(&git_dir).unwrap();
    fs::create_dir_all(root.path().join("meta/info")).unwrap();
    fs::write(worktree.join(".git"), "gitdir: ../meta/worktrees/client\n").unwrap();
    fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    fs::write(root.path().join("meta/info/exclude"), "secret.env\n").unwrap();
    fs::write(worktree.join("secret.env"), "secret\n").unwrap();

    let paths = fallback_ignored_paths(&worktree).unwrap();
    assert_eq!(paths, vec![PathBuf::from("secret.env")]);
}

#[cfg(unix)]
#[test]
fn ignored_untracked_input_is_rejected() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
    fs::write(project.path().join("secret.env"), "TOKEN=do-not-publish\n").unwrap();
    fs::write(project.path().join("public.txt"), "safe\n").unwrap();

    let error = preflight_git_ignored(project.path(), &manifest("")).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("secret.env"), "{message}");
    assert!(message.contains("package artifact"), "{message}");
    assert!(message.contains("Git ignore rules are not publication rules"));
}

#[cfg(unix)]
#[test]
fn explicit_package_ignore_allows_ignored_input() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
    fs::write(project.path().join(IGNORE_FILE), "secret.env\n").unwrap();
    fs::write(project.path().join("secret.env"), "TOKEN=local-only\n").unwrap();

    assert_eq!(
        preflight_git_ignored(project.path(), &manifest("")).unwrap(),
        1
    );
}

#[cfg(unix)]
#[test]
fn publish_exclusion_allows_ignored_input() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
    fs::write(project.path().join("secret.env"), "TOKEN=local-only\n").unwrap();

    let manifest = manifest(
        r#"[publish]
exclude = ["secret.env"]
"#,
    );
    assert_eq!(preflight_git_ignored(project.path(), &manifest).unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn polyglot_target_checks_paths_relative_to_its_source_root() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::create_dir_all(project.path().join("clients/ts")).unwrap();
    fs::write(
        project.path().join(".gitignore"),
        "clients/ts/private.key\n",
    )
    .unwrap();
    fs::write(project.path().join("clients/ts/private.key"), "private\n").unwrap();

    let error = preflight_git_ignored(
        project.path(),
        &manifest(
            r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        ),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("clients/ts/private.key"), "{message}");
    assert!(message.contains("target `nodejs` artifact"), "{message}");
}

#[cfg(unix)]
#[test]
fn polyglot_source_ignore_is_not_mistaken_for_pack_exclusion() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::create_dir_all(project.path().join("clients/ts")).unwrap();
    fs::write(
        project.path().join(".gitignore"),
        "clients/ts/private.key\n",
    )
    .unwrap();
    fs::write(
        project.path().join("clients/ts/.zedignore"),
        "private.key\n",
    )
    .unwrap();
    fs::write(project.path().join("clients/ts/private.key"), "private\n").unwrap();

    let error = preflight_git_ignored(
        project.path(),
        &manifest(
            r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        ),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("clients/ts/private.key"), "{message}");
}

#[cfg(unix)]
#[test]
fn ignored_input_outside_all_polyglot_targets_is_safe() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::create_dir_all(project.path().join("clients/ts")).unwrap();
    fs::create_dir_all(project.path().join("scratch")).unwrap();
    fs::write(project.path().join(".gitignore"), "scratch/private.key\n").unwrap();
    fs::write(project.path().join("scratch/private.key"), "private\n").unwrap();

    let manifest = manifest(
        r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
    );
    assert_eq!(preflight_git_ignored(project.path(), &manifest).unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn tracked_file_matching_gitignore_is_not_treated_as_local_input() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join("tracked.env"), "published=true\n").unwrap();
    git(project.path(), &["add", "--", "tracked.env"]);
    fs::write(project.path().join(".gitignore"), "tracked.env\n").unwrap();

    assert_eq!(
        preflight_git_ignored(project.path(), &manifest("")).unwrap(),
        0
    );
}

#[test]
fn non_git_source_tree_is_not_forced_to_have_git() {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("payload.txt"), "runtime\n").unwrap();
    assert_eq!(
        preflight_git_ignored(project.path(), &manifest("")).unwrap(),
        0
    );
}
