//! Shared deletion safeguards for doty mutation paths.
//!
//! Every filesystem removal in doty must pass through [`GuardedPath`]:
//! symlink refusal, mount-boundary refusal, special-file refusal, and
//! protection-marker checks. Callers perform their own age/ownership policy;
//! this module owns only the mechanisms that prevent deleting the wrong
//! thing.

use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// Marker file: any directory containing it (or below a directory
/// containing it, up to the allowed root) protects the whole subtree.
pub const PROTECT_MARKER: &str = ".doty-protect";

/// A path that has passed all structural safety checks. Construct only via
/// [`guard_path`]; the fields are evidence, not promises across time —
/// callers must re-guard after any await point or before mutation.
#[derive(Debug, Clone)]
pub struct GuardedPath {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub is_dir: bool,
}

/// Guard `path` for deletion under `allowed_root`.
///
/// Fails closed on: missing path, symlinks anywhere in the resolved chain,
/// mount boundaries (dev differs from the root), special files, protection
/// markers on the path or any ancestor up to the root, and git repository
/// markers (`.git` file or dir) on the path itself.
pub fn guard_path(path: &Path, allowed_root: &Path) -> Result<GuardedPath> {
    let root = allowed_root
        .canonicalize()
        .with_context(|| format!("cannot resolve allowed root {}", allowed_root.display()))?;
    if !root.is_dir() {
        anyhow::bail!("allowed root {} is not a directory", root.display());
    }
    let root_dev = root
        .metadata()
        .with_context(|| format!("cannot stat allowed root {}", root.display()))?
        .dev();

    // Reject `..` escapes and absolute-path games before touching the fs.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                anyhow::bail!("refusing path with `..`: {}", path.display())
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                // Absolute paths are fine; anchor them for the containment check.
                normalized.push(component.as_os_str());
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    let candidate = if normalized.is_absolute() {
        normalized
    } else {
        root.join(normalized)
    };

    // Walk each component at or below the root with symlink_metadata:
    // no component may be a symlink, and nothing below the root may
    // cross onto another device. Ancestors above the root are irrelevant:
    // containment below already prevents escape.
    let relative = candidate
        .strip_prefix(&root)
        .with_context(|| format!("cannot relativize {}", candidate.display()))?;
    let mut current = root.clone();
    for component in relative.components() {
        current.push(component.as_os_str());
        let meta = std::fs::symlink_metadata(&current)
            .with_context(|| format!("cannot stat {}", current.display()))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing symlink component: {}", current.display());
        }
        if meta.dev() != root_dev {
            anyhow::bail!("refusing mount boundary at {}", current.display());
        }
        if current != candidate && !meta.is_dir() {
            anyhow::bail!("refusing non-directory ancestor: {}", current.display());
        }
    }

    let meta = std::fs::symlink_metadata(&candidate)
        .with_context(|| format!("cannot stat {}", candidate.display()))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing symlink target: {}", candidate.display());
    }
    if !meta.is_file() && !meta.is_dir() {
        anyhow::bail!("refusing special file: {}", candidate.display());
    }

    // Containment: the candidate must live under the allowed root.
    if !candidate.starts_with(&root) {
        anyhow::bail!(
            "refusing path outside allowed root: {}",
            candidate.display()
        );
    }

    // Protection markers and git repositories on the path or any ancestor
    // up to (not including) the filesystem root of the allowed tree.
    let mut cursor: Option<&Path> = Some(candidate.as_path());
    while let Some(dir) = cursor {
        if dir.join(PROTECT_MARKER).is_file() {
            anyhow::bail!("refusing protected path: {}", candidate.display());
        }
        if dir.join(".git").exists() {
            anyhow::bail!("refusing git repository content: {}", candidate.display());
        }
        if dir == root {
            break;
        }
        cursor = dir.parent();
    }

    Ok(GuardedPath {
        path: candidate,
        dev: meta.dev(),
        ino: meta.ino(),
        is_dir: meta.is_dir(),
    })
}

/// Re-validate a previously guarded path: same device, inode, and kind.
/// Returns the fresh guard on success.
pub fn revalidate(guarded: &GuardedPath, allowed_root: &Path) -> Result<GuardedPath> {
    let fresh = guard_path(&guarded.path, allowed_root)?;
    if fresh.dev != guarded.dev || fresh.ino != guarded.ino || fresh.is_dir != guarded.is_dir {
        anyhow::bail!(
            "path identity changed since planning: {}",
            guarded.path.display()
        );
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().expect("fixture dir")
    }

    #[test]
    fn refuses_symlink_target() -> Result<()> {
        let root = fixture();
        let outside = fixture();
        let real = outside.path().join("real.txt");
        std::fs::write(&real, "data")?;
        symlink(&real, root.path().join("link.txt"))?;
        let err = guard_path(&root.path().join("link.txt"), root.path()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        Ok(())
    }

    #[test]
    fn refuses_symlink_ancestor_swap() -> Result<()> {
        let root = fixture();
        let subdir = root.path().join("sub");
        std::fs::create_dir(&subdir)?;
        std::fs::write(subdir.join("file.txt"), "data")?;
        // Replace the directory with a symlink after creation.
        std::fs::remove_dir_all(&subdir)?;
        symlink(fixture().path(), &subdir)?;
        let err = guard_path(&subdir.join("file.txt"), root.path()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        Ok(())
    }

    #[test]
    fn refuses_git_repository() -> Result<()> {
        let root = fixture();
        let repo = root.path().join("checkout");
        std::fs::create_dir_all(repo.join(".git"))?;
        std::fs::write(repo.join("work.txt"), "data")?;
        let err = guard_path(&repo.join("work.txt"), root.path()).unwrap_err();
        assert!(err.to_string().contains("git repository"), "{err}");
        let err = guard_path(&repo, root.path()).unwrap_err();
        assert!(err.to_string().contains("git repository"), "{err}");
        Ok(())
    }

    #[test]
    fn refuses_protected_marker() -> Result<()> {
        let root = fixture();
        let subdir = root.path().join("protected");
        std::fs::create_dir_all(&subdir)?;
        std::fs::write(subdir.join(PROTECT_MARKER), "operator hold")?;
        std::fs::write(subdir.join("data.txt"), "data")?;
        let err = guard_path(&subdir.join("data.txt"), root.path()).unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        Ok(())
    }

    #[test]
    fn refuses_parent_escape() -> Result<()> {
        let root = fixture();
        let err = guard_path(Path::new("../outside"), root.path()).unwrap_err();
        assert!(err.to_string().contains("`..`"), "{err}");
        Ok(())
    }

    #[test]
    fn refuses_special_files() -> Result<()> {
        let root = fixture();
        let socket_path = root.path().join("test.sock");
        // UnixListener::bind creates a socket file with std only.
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
        let err = guard_path(&socket_path, root.path()).unwrap_err();
        assert!(err.to_string().contains("special file"), "{err}");
        Ok(())
    }

    #[test]
    fn revalidate_detects_replacement() -> Result<()> {
        let root = fixture();
        let file = root.path().join("data.txt");
        std::fs::write(&file, "v1")?;
        let guarded = guard_path(&file, root.path())?;
        std::fs::remove_file(&file)?;
        std::fs::write(&file, "v2")?;
        let err = revalidate(&guarded, root.path()).unwrap_err();
        assert!(err.to_string().contains("identity changed"), "{err}");
        Ok(())
    }
}
