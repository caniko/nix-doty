# User and system housekeeping

Doty defaults to **user mode**, including when an explicit `--config` is supplied.
It reads `$XDG_CONFIG_HOME/doty/targets.json` (normally `~/.config/doty/targets.json`).
System maintenance selects its policy explicitly:

```sh
doty status
doty run --apply
doty --mode system status
doty --mode system run --apply
```

The NixOS service selects system mode and runs as root. Selecting system mode in
an interactive shell preserves the existing provider privilege behavior; it
does not itself grant privileges. Older target documents without a `mode` field
are system policy and require `--mode system`.

## User boundary

User mode admits only providers with an implemented caller-local contract:
`package-caches` (guarded retention), `uv-cache/clean`, and
`freedesktop-trash/empty`. Other providers are system-only. `list` shows the
providers available in the selected mode. Confirm/Risky variants still require
`--apply --force`; ReportOnly variants never mutate, even with `--force`.

Home and XDG roots must be caller-owned and have real, non-shared-writable
ancestors. Paths must be absolute and normalized. Before recursive inspection
or mutation, Doty checks ownership, writable modes, hard links, special files,
and mounted descendants within a bounded scan. Symlink leaves are never followed;
trash may unlink them. A foreign, inaccessible, or over-budget tree fails closed.
This is policy enforcement within the caller's Unix permissions, not a sandbox
against other processes running with the same UID.

An external caller-owned directory can be admitted explicitly:

```sh
doty --user-root /data/dejana/projects worktree remove --closeout /data/dejana/projects/evidence/removal-readiness.json
```

`--user-root` cannot admit another user's directory. The configured
`CHAOSBOX_SCRATCH_ROOT` is also admitted after ownership checks. Scratch custody
uses the existing Chaosbox settings. `rm`, `restore`, `purge`, and worktree removal
check caller containment and ownership, including recorded plan destinations and
Git's common directory. The rm state defaults to `$XDG_STATE_HOME/doty`.

UV inspection and `uv cache clean --cache-dir ...` use the same cache. In user
mode its default is `$XDG_CACHE_HOME/uv`, independently of a shared `UV_CACHE_DIR`
environment variable. Shared caches remain system policy. Trash defaults to
`$XDG_DATA_HOME/Trash`; additional volume trash locations can be configured using
`settings.paths` and require caller ownership.

## Home Manager

Import `doty.homeManagerModules.default` (also exported as `homeModules.default`):

```nix
programs.doty = {
  enable = true;
  targets = {
    uv-cache = {
      enable = true;
      variants = [{variant = "clean";}];
    };
    freedesktop-trash = {
      enable = true;
      variants = [{variant = "empty";}];
    };
  };
};
```

This installs Doty and renders a user-mode document. The optional
`programs.doty.timer.enable` defaults to false. Its user service selects user
mode, passes the generated config and XDG directories, and applies Safe-tier
targets only. Confirm-tier trash and retention require manual approval.

## Migration for administrators

Add `--mode system` to existing commands that consume `/etc/doty/targets.json`,
including `doctor`, `reclaim`, and system scratch housekeeping. Keep user policy
in Home Manager and system policy in NixOS. A mode mismatch is an error rather
than an implicit fallback to broader policy.
