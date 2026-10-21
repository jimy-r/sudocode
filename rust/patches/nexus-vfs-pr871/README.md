# PR 871 dependency change

This patch extends the existing `nexus-vfs v0.8.22` dependency with request-scoped resource admission and the version 1 `ScopedFs` RPC. `source.json` records the exact base commit and SHA-256 of `changes.patch`.

The runtime changes require this dependency patch. The published `v0.8.22` dependency does not provide the new interfaces. The main workspace release pin stays unchanged until a real dependency release and matching daemon are available; this bundle is for implementation review and local verification.

Use a clean dependency checkout at `c7489d3b772602d91106e8d6f65c8758e9479040` (tag `v0.8.22`), then run from the sudocode repository:

```sh
python rust/patches/nexus-vfs-pr871/verify.py /absolute/path/to/nexus-vfs
```

The helper checks the base commit and patch checksum, applies the patch, generates an ignored Cargo patch configuration under `rust/target`, runs the runtime filesystem regression suite, and restores the main release lockfile. It accepts a Cargo command after the dependency path, for example:

```sh
python rust/patches/nexus-vfs-pr871/verify.py /absolute/path/to/nexus-vfs check -p engine-host
```

Each invocation requires a fresh clean baseline checkout; the helper rejects existing local modifications. Run verification commands sequentially because the helper temporarily patches the dependency graph and restores the same lockfile. Linux executes the real mTLS transport and native symlink cases. Windows executes the applicable filesystem cases.

The patch preserves existing nonrestricted interfaces. Native path checks cover stable symlinks; concurrent replacement of native links is outside this change. Unsupported restricted peer/backend forwarding is rejected. A compound operation can have an admitted partial effect if a later step fails; no new rollback guarantee is provided.
