# Finalized worktree removal

`doty worktree remove` verifies an existing closeout manifest and removes its
registered linked checkout through ordinary `git worktree remove`. Run it from
outside the checkout being removed.

```sh
doty worktree remove --closeout /absolute/evidence/removal-readiness.json
doty worktree remove --closeout /absolute/evidence/removal-readiness.json --apply
```

The default is a read-only preview. Both invocations recheck the registered
owning repository, candidate HEAD/tree, retained branch, tracked and untracked
source changes, Git operation/registration locks, `.doty-protect` markers,
mounts and special files. Ignored caches may be removed along with the checkout;
protection markers inside ignored directories still block removal. Symlinks
within checkout content are inspected without following them.

Process checks require Linux. They inspect command arguments, cwd, executable,
file descriptors and mappings for the current user's processes (all visible
users when run as root). Detected checkout references block removal. Some
non-dumpable services hide references even from their own user; their PIDs are
reported in `limited_process_inspection`. Other users' processes and references
hidden by `/proc` permissions are outside an unprivileged scan's coverage.
These checks are point-in-time observations, not a lock against new workers.

Apply rechecks the manifest, checkout and artifact inventory after any linkage,
then invokes Git without force. It verifies directory/registration removal and
branch retention afterward. The owning checkout's source/index and external
evidence are preserved. Errors exit nonzero; `--json` prints the successful
preview or removal report, including `removed`, verified artifact count,
closeout SHA-256 and optional linkage result.

## Closeout manifest

Use the existing `removal-readiness.json` shape. Required fields are:

| Field | Meaning |
| --- | --- |
| `readyForRemoval` | Must be `true`; live checks still run |
| `worktree` | Absolute canonical path to the linked checkout |
| `owningRepository` | Absolute canonical path to its main owning checkout |
| `candidate` | Exact commit object ID |
| `candidateTree` | Exact tree object ID |
| `branch` | Retained local branch name, without `refs/heads/` |
| `handoff` | Absolute path to a handoff included in `artifacts` |
| `artifacts` | Relative filenames mapped to `{ "sha256": "…", "bytes": N }` |

Artifacts live beneath the manifest directory, outside the worktree. Paths
containing symlink components or `..` are rejected. Each artifact must be a
regular file with its recorded size and SHA-256. The inventory must contain
`source.bundle`: a verified standalone Git bundle with no history prerequisites,
containing the candidate's exact branch ref. The closeout itself is bounded to
four MiB. Additional manifest fields, including pending CI/review and production
obligations, remain in the original evidence document.

Removal readiness does not establish PR qualification or completion. Keep the
handoff current before creating its artifact digest.

## Optional Chaosbox preservation link

Supply an **existing recorded allocation** in the configured scratch scope:

```sh
doty worktree remove --closeout /absolute/evidence/removal-readiness.json \
  --chaosbox-allocation /data/scratch/tmp/opencode/recorded-allocation
```

Use Doty's existing global `--chaosbox-work`, `--chaosbox-scope`,
`--chaosbox-host`, `--chaosbox-root`, `--chaosbox-bin` options or matching
`CHAOSBOX_SCRATCH_*` environment settings. Custody must be outside the removed
checkout. The facade must support `scratch query` and `scratch link`.

Preview queries allocation identity and reports the planned link without
writing it. Apply records a `preservation` reference to the closeout, with its
SHA-256, candidate, branch and handoff in the description, and reads the exact
link back before removal. A missing/replaced allocation, failed write or
mismatched readback stops removal. An identical existing link is reused.

The link remains a Chaosbox assertion; it does not release an allocation or
satisfy pending obligations. If later checks or Git removal fail, the
preservation assertion remains recorded and the command reports the failure.

Git and `sha256sum` are provided by the Nix package's runtime wrapper.
