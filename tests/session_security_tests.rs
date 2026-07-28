use std::fs;
use std::path::Path;
use std::sync::Arc;

use mc_server_download_tool::i18n::Language;
use mc_server_download_tool::install::{
    InstallError, InstallEvent, InstallObserverError, InstallRoot, InstallSession,
};
use mc_server_download_tool::loader::ProcessStream;

fn manifest(root: &Path) -> std::path::PathBuf {
    let path = root.join("server-install.json");
    fs::write(&path, b"{}").unwrap();
    path
}

#[test]
fn session_rejects_metadata_directory_linked_outside_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let manifest = manifest(root.path());
    if !create_directory_link(outside.path(), &root.path().join(".mcsdt")) {
        return;
    }

    let result = InstallSession::acquire(
        &manifest,
        Language::EnUs,
        Arc::new(|_| {}),
        std::iter::empty(),
    );

    assert!(matches!(result, Err(InstallError::UnsafePath { .. })));
    assert!(!outside.path().join("install.lock").exists());
    assert!(!outside.path().join("install.log").exists());
}

#[test]
fn root_rejects_linked_parent_and_linked_file_targets() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let manifest = manifest(root.path());
    let boundary = InstallRoot::from_manifest(&manifest).unwrap();

    fs::write(outside.path().join("outside.jar"), b"outside").unwrap();
    fs::create_dir(root.path().join("mods")).unwrap();
    let file_link_created = create_file_link(
        &outside.path().join("outside.jar"),
        &root.path().join("mods/linked.jar"),
    );
    if file_link_created {
        assert!(matches!(
            boundary.resolve(Path::new("mods/linked.jar")),
            Err(InstallError::UnsafePath { .. })
        ));
    }

    let parent_link_created =
        create_directory_link(outside.path(), &root.path().join("linked-parent"));
    if parent_link_created {
        assert!(matches!(
            boundary.resolve(Path::new("linked-parent/server.properties")),
            Err(InstallError::UnsafePath { .. })
        ));
    }
}

#[test]
fn session_log_is_synchronized_and_redacts_sensitive_values() {
    let root = tempfile::tempdir().unwrap();
    let manifest = manifest(root.path());
    let secret = "curseforge-test-secret";
    let session = InstallSession::acquire(
        &manifest,
        Language::EnUs,
        Arc::new(|_| {}),
        [secret.to_string()],
    )
    .unwrap();
    let observer = session.observer();
    let mut workers = Vec::new();
    for worker in 0..8 {
        let observer = Arc::clone(&observer);
        workers.push(std::thread::spawn(
            move || -> Result<(), InstallObserverError> {
            for line in 0..20 {
                observer.observe(InstallEvent::LoaderOutput {
                    stream: ProcessStream::Stdout,
                    line: format!(
                        "worker={worker} line={line} key={secret} url='https://user:pass@example.com/file?token={secret}#part'"
                    ),
                })?;
            }
            Ok(())
        },));
    }
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    session.check_log().unwrap();

    let log = fs::read_to_string(root.path().join(".mcsdt/install.log")).unwrap();
    assert!(!log.contains(secret));
    assert!(!log.contains("user:pass"));
    assert!(!log.contains("?token="));
    assert!(!log.contains("#part"));
    assert_eq!(
        log.lines().filter(|line| line.contains("worker=")).count(),
        160
    );
}

#[test]
fn log_creation_failure_aborts_before_installation_work() {
    let root = tempfile::tempdir().unwrap();
    let manifest = manifest(root.path());
    fs::create_dir(root.path().join(".mcsdt")).unwrap();
    fs::create_dir(root.path().join(".mcsdt/install.log")).unwrap();

    let result = InstallSession::acquire(
        &manifest,
        Language::EnUs,
        Arc::new(|_| {}),
        std::iter::empty(),
    );

    assert!(matches!(result, Err(InstallError::Io { .. })));
}

#[cfg(unix)]
fn create_file_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(windows)]
fn create_file_link(target: &Path, link: &Path) -> bool {
    std::os::windows::fs::symlink_file(target, link).is_ok()
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) -> bool {
    std::os::windows::fs::symlink_dir(target, link).is_ok()
}
