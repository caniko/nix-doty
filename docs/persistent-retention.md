# Persistent retention

Doty can inspect and reclaim explicitly configured persistent build outputs,
package/compiler caches and retired model directories. Legacy databases and
campaign copies use a separate quarantine workflow.

| Target | Variant | Selection |
| --- | --- | --- |
| `cargo-builds` | `disk-report` | Footprint of configured Cargo targets |
| `cargo-builds` | `prune-incremental` | Stale incremental crate directories in native or cross profiles |
| `cargo-builds` | `prune-targets` | Entire stale, proven Cargo target directories |
| `package-caches` | `disk-report` | Footprint of configured cache entries |
| `package-caches` | `prune-stale` | Stale entries at the configured depth |
| `model-directories` | `disk-report` | Footprint and retirement/pin decisions |
| `model-directories` | `prune-retired` | Explicit retired paths, excluding pins |
| `retained-state` | `disk-report` | Legacy/campaign footprint and eligibility |
| `retained-state` | `quarantine-approved` | Exact approved paths, retained for restore or later purge |

Reports never mutate. Every other variant is confirm-tier and requires both
`--apply` and `--force`. The scheduled `run --apply` skips them.

## Configuration

These variants share the following settings. Unknown fields are rejected.

| Setting | Default | Meaning |
| --- | --- | --- |
| `roots` | `[]` | Absolute, existing real directories that bound removal; roots cannot overlap |
| `paths` | `[]` | Absolute paths strictly below roots; paths cannot overlap |
| `minAgeDays` | `30` | Minimum age of the newest timestamp anywhere in a candidate |
| `maxEntries` | `1000000` | Report-wide discovery/metadata budget, including directory entries |
| `maxDepth` | `64` | Maximum candidate scan depth |
| `entryDepth` | `1` | Cache candidate depth below each path, from 1 through 4 |
| `pinnedPaths` | `[]` | Protect paths and any overlapping candidates |
| `retiredPaths` | `[]` | Exact model paths eligible for retirement |
| `approvedPaths` | `[]` | Exact legacy/campaign paths approved for quarantine |

An empty selection is a no-op. Missing configured paths are reported and skipped.
Use roots on the same filesystem as their candidates. No symlink is followed.
`tmp`, `.tmp`, `.git`, `.doty-protect` and `.doty-quarantine` subtrees are excluded.
Any candidate containing one of these, a symlink, special file, mount boundary,
unknown age, unreadable content or an incomplete scan is preserved. Independently
complete sibling candidates may still be reclaimed.

Cargo targets require a valid `CACHEDIR.TAG` or a real `debug/.fingerprint` or
`release/.fingerprint` directory. Incremental retention preserves `deps`,
fingerprints and other outputs. Keep persistent build roots outside repositories;
repository protection also applies to incremental children.

For caches, choose only regenerable cache namespaces. Doty does not understand
arbitrary package indexes or application data. Cache checks include references to
the entire configured cache path so, for example, an open SQLite database protects
its companion journal files. Pins can retain current compiler caches.

The data owner's live `/proc` cwd, open-file and memory-map references protect
candidates. Root invocations also inspect root-owned processes. Unobservable
process use preserves the candidate; bounded diagnostic samples name the process
whose evidence was unavailable. Run inspections/mutations with sufficient process
visibility (usually root on a desktop with non-dumpable processes). Process scans
have a one-million-reference budget per owner and a 16 MiB per-process map limit.
For incremental retention, this check covers
the entire Cargo target, including Cargo's open build lock. This is a point-in-time
check, not a lock shared with every producer: keep producers idle during cleanup.

```nix
services.doty.targets.cargo-builds = {
  enable = true;
  variants = [{
    variant = "prune-incremental";
    settings = {
      roots = ["/srv/builds"];
      paths = ["/srv/builds/example/cargo"];
      minAgeDays = 7;
    };
  }];
};
```

```sh
doty status --target cargo-builds --variant prune-incremental --json
doty run --target cargo-builds --variant prune-incremental
doty run --target cargo-builds --variant prune-incremental --apply --force
```

## Retirement and recovery

Populate `approvedPaths` only after verifying current preservation and cutover
evidence. Doty treats this exact allowlist as operator policy; it does not infer
cutover completion from directory age or parse application-specific receipts.

`quarantine-approved --apply --force` records a crash-recoverable removal plan and
renames entries into a same-filesystem quarantine. It prints the plan ID. The
bytes remain allocated until a separate purge:

```sh
doty restore --plan PLAN_ID
doty purge --plan PLAN_ID
doty purge --plan PLAN_ID --apply
```

Regenerable caches, Cargo outputs and retired models use that same journaled,
contained executor, then purge in the authorized invocation. Descendant metadata
is rechecked before mutation and after quarantine. A failed recheck retains the
quarantine with its recovery journal. `--force` never bypasses these checks.

## Accounting

Reports expose logical footprint, allocated blocks and eligible candidate paths
in their notes. Hardlinks are deduplicated within each candidate tree for allocated
block accounting. Reflinks, cross-tree hardlinks, sparse allocation and compression
make these footprints unsuitable as additive reclaim estimates. Mutating retention
inspections therefore leave `size_bytes` unknown; quarantine reports zero potential
immediate relief. Reclaim still schedules eligible paths using `would_remove`.

`freed_bytes` is the observed positive filesystem-available-space delta, deduplicated
by device, after a purge. Concurrent filesystem activity can affect that delta;
it is not proof that all of it came from Doty. Quarantine always reports zero.

The flake exports `lib.persistentRetention = true` for downstream capability gating
while an older Doty revision remains pinned.
