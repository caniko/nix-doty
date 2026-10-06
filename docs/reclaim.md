# Goal-driven reclaim

`doty reclaim` plans cleanup using the installed target policy. Use `--apply`
to execute it. Only configured targets affecting selected backing filesystems
are eligible; tier gates, retained pins and scratch custody checks still apply.
Mount aliases and Btrfs subvolumes sharing backing storage get one free-space
goal. Invalid mount paths fail rather than reporting successful empty plans.

```sh
doty reclaim --mount /nix --min-free-bytes 107374182400
doty reclaim --mount /nix --min-free-bytes 107374182400 --apply \
  --timeout-seconds 1200 --gc-max-bytes 34359738368 --gc-pass-bytes 4294967296
```

Canix forwards these options through `canix host maintenance reclaim atlas`.
Explicit byte or percentage goals take precedence over the default 85% usage
threshold. Choose one explicit goal. `--all` includes already-healthy filesystems
in the report; execution skips their actions when the goal is already satisfied.
Known-yield actions sort ahead of unknown-yield actions within the same tier.

## Measurements and results

The execution baseline is sampled immediately before acting. Cheap `statvfs`
counters are read after every action and again at completion. A goal is met only
when the exact available-byte counter reaches the required value. Human output
includes exact bytes alongside rounded sizes and the remaining shortfall.

- **Adapter-reported yield** is what the cleanup backend measured, or `unknown`.
  Nix reports logical bytes deleted from the store.
- **Net available-space change** is the signed change in filesystem availability.
  It can be negative when concurrent writes exceed cleanup, and is not attributed
  solely to Doty. Compression, shared extents and hard links can also make it
  differ from Nix's byte count.
- **Threshold health** describes the separate usage threshold using final counters.
- Unknown estimates are displayed as `unknown`. Aggregate estimates count known
  yields and disclose how many unknown yields remain.

Applied reclaim returns nonzero if any action fails, a measurement is unavailable,
the runtime is interrupted, or a selected filesystem's goal remains unmet.
The final report is printed before returning failure. Dry-run plans return success
even when known estimates fall short. Empty plans preserve the requested JSON format.

## Nix retention and budgets

Configure `nix-store-gc/nh-clean` with optional settings:

```nix
services.doty.targets.nix-store-gc = {
  enable = true;
  variant = "nh-clean";
  settings = { scope = "all"; keep = 3; keepSince = "14d"; };
};
```

`scope` is `all` (default) or `user`. For `all`, a non-root caller first validates
sudo credentials; an interactive terminal gets the normal password prompt, while
noninteractive execution requires existing credentials or passwordless sudo.
Failure occurs before profile pruning. Subsequent privileged commands use `sudo -n`.

Profile retention runs once with `nh clean --no-gc --no-gcroots --no-direnv`.
GC then runs in bounded `nix-store --gc --max-freed` passes, checking physical
availability after each pass. Stop conditions are:

1. The free-space goal is reached.
2. GC completes below its requested byte limit (no further unreachable paths).
3. The total logical-byte budget or runtime budget is exhausted.
4. A command fails or Nix's byte summary is unavailable.

Defaults are 20 minutes, 32 GiB total logical GC bytes, and 4 GiB requested per
pass. The total GC budget is shared across selected Nix targets. Nix's
[`--max-freed`](https://nix.dev/manual/nix/2.34/command-ref/nix-store/gc)
stops after *at least* the requested logical bytes: the last deleted path can
overshoot a pass or total budget. Logical limits do not guarantee equivalent
physical space recovery. A zero-byte completed pass is successful GC, although
the overall requested goal may still be unmet.

The `nix-collect-garbage` reclaim variant runs bounded GC without pruning profile
generations. Its standalone `doty run` behavior retains generation deletion.
Store optimisation has unmeasured yield and uses the same runtime budget.

## Progress, interruption and JSON

Subprocess progress goes to stderr, keeping JSON stdout machine-readable.
Each subprocess retains up to 32 KiB from each output stream; the Nix action
retains up to 64 KiB across successful commands, with failure tails in error notes.
SIGINT/SIGTERM and timeouts cancel the subprocess group, allow up to three seconds
for graceful shutdown, then kill remaining children. A final availability sample
and a nonzero result preserve evidence of partial cleanup. Runtime checks happen
between filesystem-local actions; subprocess-based actions share the deadline.
In-process filesystem work finishes its current action before the next check.
Adapters consuming semantic stdout fail if its capture budget is exceeded;
truncated command data is never silently used for cleanup decisions.

Reclaim **schema version 3** changes target `estimated_freed_bytes` to an optional
integer (`null` means unknown). Consumers should also handle optional
`actual_freed_bytes`, `goal_met` and `final_available_bytes`. New fields include
`required_available_bytes`, signed `net_available_change_bytes`, measurement
errors, bounded command logs, aggregate unknown-estimate counts and runtime errors.
The legacy `target_free_bytes` field remains the initial additional-space goal.

Activation must consume a qualified Doty source revision. The Nix capability
`lib.goalDrivenReclaimVersion = 1` lets hosts gate updated Nix target policy until
that revision is available.
