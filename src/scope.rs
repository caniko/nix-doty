//! Execution policy is independent of safety tiers and configuration selection.
use anyhow::{Context as _, Result, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    User,
    System,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserScope {
    SystemOnly,
    Paths,
    Trash,
    UvCache,
}

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub mode: Mode,
    uid: u32,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub cache_home: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
    roots: Vec<PathBuf>,
}

impl ExecutionContext {
    pub fn system() -> Self {
        Self {
            mode: Mode::System,
            uid: 0,
            home: PathBuf::new(),
            config_home: PathBuf::new(),
            cache_home: PathBuf::new(),
            data_home: PathBuf::new(),
            state_home: PathBuf::new(),
            roots: Vec::new(),
        }
    }

    pub fn current(mode: Mode, extra_roots: &[PathBuf]) -> Result<Self> {
        if mode == Mode::System {
            ensure!(extra_roots.is_empty(), "--user-root requires user mode");
            return Ok(Self::system());
        }
        // SAFETY: identity queries have no arguments or side effects.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            uid == unsafe { libc::getuid() },
            "setuid execution is unsupported"
        );
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("user mode requires HOME")?;
        let xdg = |name: &str, suffix: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(suffix))
        };
        let mut context = Self {
            mode,
            uid,
            config_home: xdg("XDG_CONFIG_HOME", ".config"),
            cache_home: xdg("XDG_CACHE_HOME", ".cache"),
            data_home: xdg("XDG_DATA_HOME", ".local/share"),
            state_home: xdg("XDG_STATE_HOME", ".local/state"),
            home: home.clone(),
            roots: vec![home],
        };
        context.check_ancestry(&context.home, true)?;
        ensure!(
            fs::symlink_metadata(&context.home)?.is_dir(),
            "HOME must be a caller-owned directory"
        );
        let scratch_root = std::env::var_os("CHAOSBOX_SCRATCH_ROOT").map(PathBuf::from);
        for root in [
            &context.config_home,
            &context.cache_home,
            &context.data_home,
            &context.state_home,
        ]
        .into_iter()
        .chain(extra_roots)
        .chain(scratch_root.as_ref())
        {
            context.check_ancestry(root, true)?;
            context.roots.push(root.clone());
        }
        Ok(context)
    }

    pub fn config_path(&self, explicit: Option<String>) -> Result<String> {
        explicit.map_or_else(
            || {
                if self.mode == Mode::System {
                    Ok(crate::config::DEFAULT_CONFIG_PATH.into())
                } else {
                    Ok(self
                        .config_home
                        .join("doty/targets.json")
                        .to_str()
                        .context("non-UTF8 config path")?
                        .to_owned())
                }
            },
            Ok,
        )
    }

    pub fn scratch_root(&self) -> PathBuf {
        if self.mode == Mode::System {
            crate::rm::DEFAULT_SCRATCH_ROOT.into()
        } else {
            std::env::var_os("CHAOSBOX_SCRATCH_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| self.cache_home.join("doty/scratch"))
        }
    }

    pub fn admit_document(&self, document: &crate::config::TargetDocument) -> Result<()> {
        let mode = document.mode.unwrap_or(Mode::System);
        ensure!(
            mode == self.mode,
            "target document is {mode:?} policy; select --mode {} with its corresponding configuration",
            if mode == Mode::System {
                "system"
            } else {
                "user"
            }
        );
        Ok(())
    }

    /// Check lexical containment and every existing ancestor, without following links.
    /// Nonexistent suffixes are permitted for not-yet-created caches/state.
    pub fn admit_path(&self, path: &Path) -> Result<()> {
        if self.mode == Mode::System {
            return Ok(());
        }
        ensure!(
            self.roots.iter().any(|root| path.starts_with(root)),
            "user path is outside caller roots: {}",
            path.display()
        );
        self.check_ancestry(path, false)
    }

    fn check_ancestry(&self, path: &Path, root: bool) -> Result<()> {
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
            "requires an absolute normalized path: {}",
            path.display()
        );
        let mut current = PathBuf::new();
        let mut missing = false;
        for part in path.components() {
            current.push(part);
            if missing {
                continue;
            }
            let meta = match fs::symlink_metadata(&current) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    missing = true;
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot inspect {}", current.display()));
                }
            };
            ensure!(
                !meta.file_type().is_symlink(),
                "user path has a symlink ancestor: {}",
                current.display()
            );
            let inside = self.roots.iter().any(|r| current.starts_with(r));
            ensure!(
                meta.uid() == self.uid
                    || (!inside
                        && current != path
                        && (meta.uid() == 0
                            || (current == Path::new("/") && unmapped_namespace_root(meta.uid())))),
                "path is not caller-owned: {}",
                current.display()
            );
            // Sticky shared ancestors (/tmp) cannot rename a caller-owned anchor.
            ensure!(
                meta.mode() & 0o022 == 0
                    || (!inside && current != path && meta.is_dir() && meta.mode() & 0o1000 != 0),
                "path is writable by other users: {}",
                current.display()
            );
            ensure!(
                current == path || meta.is_dir(),
                "non-directory ancestor: {}",
                current.display()
            );
        }
        if root && missing {
            // External roots must have an existing caller-owned anchor. Missing
            // default XDG directories inherit the already-admitted home anchor.
            ensure!(
                path.starts_with(&self.home),
                "external user root must exist: {}",
                path.display()
            );
        }
        Ok(())
    }

    /// Bound ownership/mount inspection before providers recursively inspect or mutate.
    /// Symlink leaves may be unlinked by trash/Git; they are never traversed here.
    pub fn admit_tree(&self, path: &Path) -> Result<()> {
        self.admit_path(path)?;
        if self.mode == Mode::System {
            return Ok(());
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut stack = vec![(path.to_owned(), 0)];
        let mut budget = 100_000usize;
        while let Some((entry, depth)) = stack.pop() {
            ensure!(
                budget > 0 && depth <= 128,
                "user ownership inspection budget exhausted"
            );
            budget -= 1;
            let m = fs::symlink_metadata(&entry)?;
            ensure!(
                m.uid() == self.uid && m.dev() == meta.dev(),
                "foreign-owned or cross-device entry: {}",
                entry.display()
            );
            ensure!(
                m.file_type().is_symlink() || m.is_file() || m.is_dir(),
                "special user entry: {}",
                entry.display()
            );
            if !m.file_type().is_symlink() {
                ensure!(
                    m.mode() & 0o022 == 0,
                    "entry writable by other users: {}",
                    entry.display()
                );
                ensure!(
                    !m.is_file() || m.nlink() == 1,
                    "hard-linked user entry: {}",
                    entry.display()
                );
            }
            if m.is_dir() {
                for child in fs::read_dir(&entry)? {
                    ensure!(
                        stack.len() < budget,
                        "user ownership inspection budget exhausted"
                    );
                    stack.push((child?.path(), depth + 1));
                }
            }
        }
        #[cfg(target_os = "linux")]
        for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
            let encoded = line
                .split_whitespace()
                .nth(4)
                .context("invalid mountinfo")?;
            let mount = PathBuf::from(
                encoded
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\"),
            );
            ensure!(
                !mount.starts_with(path) || mount == path,
                "mounted user descendant: {}",
                mount.display()
            );
        }
        Ok(())
    }

    pub fn settings(&self, scope: UserScope, original: &Value) -> Result<Value> {
        if self.mode == Mode::System {
            return Ok(original.clone());
        }
        ensure!(
            scope != UserScope::SystemOnly,
            "system-only provider; select --mode system explicitly"
        );
        let mut settings = if original.is_null() {
            json!({})
        } else {
            original.clone()
        };
        ensure!(settings.is_object(), "provider settings must be an object");
        match scope {
            UserScope::Paths => {
                for field in ["roots", "paths"] {
                    let paths = settings[field]
                        .as_array()
                        .context("user retention requires explicit roots and paths")?;
                    ensure!(
                        !paths.is_empty(),
                        "user retention requires nonempty roots and paths"
                    );
                    for path in paths {
                        let path = Path::new(path.as_str().context("path must be a string")?);
                        if field == "roots" {
                            self.admit_path(path)?;
                        } else {
                            self.admit_tree(path)?;
                        }
                    }
                }
            }
            UserScope::Trash => {
                ensure!(
                    settings.as_object().unwrap().keys().all(|k| k == "paths"),
                    "unknown user trash setting"
                );
                if settings.get("paths").is_none() {
                    settings["paths"] = json!([self.data_home.join("Trash")]);
                }
                let paths = settings["paths"]
                    .as_array()
                    .context("trash paths must be an array")?;
                for path in paths {
                    self.admit_tree(Path::new(
                        path.as_str().context("trash path must be a string")?,
                    ))?;
                }
            }
            UserScope::UvCache => {
                ensure!(
                    settings
                        .as_object()
                        .unwrap()
                        .keys()
                        .all(|k| k == "cacheDir"),
                    "unknown UV setting"
                );
                if settings.get("cacheDir").is_none() {
                    settings["cacheDir"] = json!(self.cache_home.join("uv"));
                }
                self.admit_tree(Path::new(
                    settings["cacheDir"]
                        .as_str()
                        .context("cacheDir must be a string")?,
                ))?;
            }
            UserScope::SystemOnly => unreachable!(),
        }
        Ok(settings)
    }

    pub fn admit_plan(&self, plan: &crate::rm::RmPlan) -> Result<()> {
        if let Some(ledger) = &plan.scratch_ledger {
            ledger.admit_scope(self)?;
        }
        self.admit_path(&plan.root)?;
        self.admit_path(&plan.quarantine_dir)?;
        for target in &plan.targets {
            self.admit_tree(&target.path)?;
            if let Some(path) = &target.quarantined_as {
                self.admit_tree(path)?;
            }
        }
        Ok(())
    }
}

/// Nix build namespaces map only their build UID: the namespace's `/` has an
/// unmapped owner. Treat that kernel-held root as an ancestor anchor, never as
/// an admitted user root. Every descendant still needs caller/root ownership.
fn unmapped_namespace_root(owner: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/uid_map")
            .ok()
            .is_some_and(|map| uid_unmapped(owner, &map))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = owner;
        false
    }
}

#[cfg(target_os = "linux")]
fn uid_unmapped(uid: u32, map: &str) -> bool {
    let mut rows = 0;
    for line in map.lines() {
        let fields = line
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>();
        let Ok(fields) = fields else {
            return false;
        };
        let [inside, _outside, count] = fields.as_slice() else {
            return false;
        };
        if *count == 0 || inside.checked_add(*count).is_none() {
            return false;
        }
        if u64::from(uid) >= *inside && u64::from(uid) < inside + count {
            return false;
        }
        rows += 1;
    }
    rows > 0
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::uid_unmapped;

    #[test]
    fn namespace_anchor_requires_an_unmapped_owner_and_valid_map() {
        assert!(uid_unmapped(65534, "1000 30011 1\n"));
        assert!(!uid_unmapped(1000, "1000 30011 1\n"));
        assert!(!uid_unmapped(65534, "0 0 4294967295\n"));
        assert!(!uid_unmapped(65534, "1000 30011 1\n65534 65534 1\n"));
        for map in ["", "invalid", "1 1 0", "1 1 18446744073709551615"] {
            assert!(!uid_unmapped(65534, map));
        }
    }
}
