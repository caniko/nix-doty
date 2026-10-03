//! Persistent inode lock shared with model download producers. Never unlink.
use anyhow::{Result, ensure};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub fn exclusive(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute()
            && !path.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )),
        "model lock must be absolute without dot components"
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serving_reader_blocks_cleanup_and_anchor_survives_release() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("model.doty-lock");
        let writer = exclusive(&path).unwrap();
        let inode = writer.metadata().unwrap().ino();
        drop(writer);
        let reader = File::open(&path).unwrap();
        // SAFETY: live file descriptor, shared read lease as held by inference.
        assert_eq!(
            unsafe { libc::flock(reader.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        assert!(exclusive(&path).is_err());
        drop(reader);
        let writer = exclusive(&path).unwrap();
        assert_eq!(writer.metadata().unwrap().ino(), inode);
    }
    #[test]
    fn symlink_anchor_and_ancestor_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("model.doty-lock");
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(exclusive(&path).is_err());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        assert!(exclusive(&alias.join("other.lock")).is_err());
    }
}
