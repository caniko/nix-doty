//! Helpers shared by the CLI integration tests.
use std::{fs, path::Path, process::Command};

/// Install an executable fixture script from a child process.
///
/// Writing it here would let another test thread's fork inherit the write
/// descriptor, and executing the script could then fail with ETXTBSY ("Text
/// file busy"). `install` holds that descriptor in its own short-lived process.
pub fn write_executable(path: &Path, contents: &str) {
    let staged = path.with_extension("staged");
    fs::write(&staged, contents).unwrap();
    let status = Command::new("install")
        .args(["-m", "0755"])
        .arg(&staged)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "install {}", path.display());
    fs::remove_file(staged).unwrap();
}
