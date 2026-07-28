#[path = "../build_support.rs"]
mod build_support;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;

use build_support::{
    BuildVersion, git_dependency_paths, release_override_from_environment, resolve_build_version,
    resolve_build_version_with_git,
};

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .status()
        .unwrap();
    assert!(status.success(), "git command failed: {arguments:?}");
}

fn commit(repository: &Path, name: &str) {
    fs::write(repository.join(name), name).unwrap();
    git(repository, &["add", name]);
    git(
        repository,
        &[
            "-c",
            "user.name=build-version-test",
            "-c",
            "user.email=build-version-test@example.invalid",
            "commit",
            "-m",
            name,
        ],
    );
}

#[test]
fn strict_release_version_rejects_metadata_prerelease_and_leading_zeroes() {
    for value in [
        "1.2",
        "1.2.3.4",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1.2.3+abc",
        "1.2.3-rc.1",
        "1.2.a",
    ] {
        assert!(BuildVersion::parse(value).is_err(), "accepted {value}");
    }
    assert_eq!(BuildVersion::parse("0.0.0").unwrap().as_str(), "0.0.0");
    assert_eq!(
        BuildVersion::parse("12.34.56").unwrap().as_str(),
        "12.34.56"
    );
}

#[test]
fn ordinary_version_uses_nearest_legal_tag_and_always_adds_head_hash() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init"]);
    commit(repository.path(), "first");
    git(repository.path(), &["tag", "v1.2.3"]);
    commit(repository.path(), "second");
    git(repository.path(), &["tag", "v2.0.0-rc.1"]);
    git(repository.path(), &["tag", "v01.0.0"]);

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repository.path())
        .output()
        .unwrap();
    assert!(head.status.success());
    let head = String::from_utf8(head.stdout).unwrap();
    let version = resolve_build_version(repository.path(), None).unwrap();
    assert_eq!(version.as_str(), format!("1.2.3+{}", &head.trim()[..7]));

    git(repository.path(), &["tag", "v2.0.0"]);
    let tagged_head_version = resolve_build_version(repository.path(), None).unwrap();
    assert_eq!(
        tagged_head_version.as_str(),
        format!("2.0.0+{}", &head.trim()[..7])
    );
}

#[test]
fn missing_legal_tag_uses_zero_baseline_and_override_does_not_need_git() {
    let _ = release_override_from_environment();
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init"]);
    commit(repository.path(), "first");
    let version = resolve_build_version(repository.path(), None).unwrap();
    assert!(version.as_str().starts_with("0.0.0+"));

    let overridden =
        resolve_build_version(repository.path(), Some(OsString::from("7.8.9"))).unwrap();
    assert_eq!(overridden.as_str(), "7.8.9");

    for invalid in ["07.8.9", "7.8.9+head", "7.8.9-rc.1"] {
        assert!(
            resolve_build_version(repository.path(), Some(OsString::from(invalid))).is_err(),
            "accepted invalid release override {invalid}"
        );
    }
}

#[test]
fn ordinary_version_does_not_change_for_a_dirty_worktree() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init"]);
    commit(repository.path(), "first");
    git(repository.path(), &["tag", "v3.4.5"]);

    let clean = resolve_build_version(repository.path(), None).unwrap();
    fs::write(repository.path().join("first"), "dirty").unwrap();
    let dirty = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(dirty, clean);
    assert!(dirty.as_str().starts_with("3.4.5+"));
}

#[test]
fn git_dependencies_include_worktree_head_and_common_tag_state() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init"]);
    commit(repository.path(), "first");
    let worktree = tempfile::tempdir().unwrap();
    git(
        repository.path(),
        &["worktree", "add", worktree.path().to_str().unwrap(), "HEAD"],
    );
    let paths = git_dependency_paths(worktree.path()).unwrap();
    assert!(paths.iter().any(|path| path.ends_with("HEAD")));
    assert!(paths.iter().any(|path| path.ends_with("packed-refs")));
    assert!(
        paths.iter().any(
            |path| path.ends_with("refs".to_string() + "\\tags") || path.ends_with("refs/tags")
        )
    );
    assert!(paths.iter().any(|path| path.ends_with(".git")));
}

#[test]
fn missing_git_or_repository_without_override_is_a_hard_error() {
    let directory = tempfile::tempdir().unwrap();
    let error = resolve_build_version(directory.path(), None).unwrap_err();
    assert!(error.to_string().contains("git"));

    let error = resolve_build_version_with_git(
        directory.path(),
        None,
        OsString::from("mcsdt-git-command-that-does-not-exist").as_os_str(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("mcsdt-git-command-that-does-not-exist")
    );
}
