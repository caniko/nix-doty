# Nix store provenance inspection

`doty nix-store` reports reasons, execution outcomes, current filesystem
presence and retention obligations for Nix objects added through Chaosbox.
It is read-only and prints the versioned Chaosbox cleanup packet as JSON.

Configure the operator-owned facade and expected namespace through
`DOTY_NIX_CHAOSBOX_BIN` (absolute path), `DOTY_NIX_SCOPE` (`private:OWNER`),
`DOTY_NIX_HOST` and optional `DOTY_NIX_STORE` (default `daemon`). The facade
must carry the matching Chaosbox journal, host, store and scope settings.

```nu
doty nix-store
doty nix-store --path /nix/store/HASH-candidate
doty nix-store --offset 20
```

The reader asks only for `chaosbox nix query`, with a 20-record page, five-second
process limit and four-MiB response limit. It checks the schema version and all
namespace dimensions, and refuses unknown retention/root-release contracts.
Missing state or malformed/incompatible packets are errors, never an empty
inventory or permission to collect.

Contract v1 additions are unrooted. Identical content may serve multiple needs,
and reasons remain in the private durable ledger after Nix collects the object.
`filesystem_presence` is not registered validity or a GC-liveness assessment;
the packet declares `gc_eligibility: unknown` and `cleanup_authorized: false`.
No provenance query deletes a store path, releases roots or runs GC. Held-root
release requires a separately qualified generation-checked Chaosbox transaction.
