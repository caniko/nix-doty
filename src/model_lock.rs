//! Persistent inode lock shared with model download producers. Never unlink.
use anyhow::{Result, ensure};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub fn exclusive(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "model lock must be absolute");
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("model lock has no parent"))?;
    for ancestor in parent.ancestors() {
        ensure!(
            std::fs::symlink_metadata(ancestor)?.is_dir(),
            "model lock ancestor is not a real directory"
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && (meta.uid() == 0 || meta.uid() == std::fs::metadata(parent)?.uid()),
        "untrusted model lock"
    );
    // SAFETY: flock only receives the live descriptor owned by this File.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "model download/use lock held: {}",
        path.display()
    );
    Ok(file)
}
