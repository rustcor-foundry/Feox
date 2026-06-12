# The Feox ecosystem: how the repositories tie together

Two repositories, one product: a capability-oriented exokernel and its
filesystem, developed in lockstep but releasable independently.

```text
rustcor/Feox                                rustcor/RFS
├── crates/feox-asi      (ASI ABI: ops,     ├── crates/rfs-core   (CoW engine:
│    args, errors — the kernel/app          │    superblock ring, B-tree, txg,
│    contract; never breaks silently)       │    ZIL, snapshots; no_std)
├── crates/feox-boot     (boot handoff)     ├── crates/rfs-feox   (BlockDevice
├── crates/feox-async    (executor)         │    over feox-nvme QueueRing)
├── crates/feox-nvme     (NVMe rings,  ◄────┤    [git dep on Feox]
│    SQE/CQE, inflight futures)             └── rfs-fuse          (Linux FUSE
├── kernel/feox-xokernel (the kernel) ────►      testbed + benches; std)
│    [git dep on RFS: rfs-core, rfs-feox,
│     behind the `rfs` feature]
└── apps/                (libOS + U-mode apps, built by the kernel's build.rs)
```

## Dependency rules

1. **RFS → Feox**: `rfs-feox` depends on `feox-nvme` by git URL. `rfs-core`
   depends on nothing of Feox's — the engine stays hardware-agnostic and is
   testable/fuzzable entirely on a host.
2. **Feox → RFS**: the kernel depends on `rfs-core` (+ its `testkit` feature
   for the poll-loop `block_on`) and `rfs-feox` by git URL, behind the
   non-default `rfs` cargo feature (the riscv64 builds enable it; x86_64
   stays alloc-free).
3. **The `[patch]` rule** (load-bearing): Feox's workspace `Cargo.toml`
   patches `rfs-feox`'s git dependency on `feox-nvme` back to the local path
   crate. Without it cargo would build TWO copies of feox-nvme (path + git)
   and the ring types would not unify. Any new cross-repo dependency on a
   Feox crate must get the same patch line.
4. **Pinning**: each repo's `Cargo.lock` pins the other repo's revision.
   Builds are reproducible; nothing moves implicitly.

## Updating one side from the other

- Feox picks up new RFS work: `cargo update -p rfs-core -p rfs-feox` in
  Feox, in its own PR — the QEMU smoke boot (which formats/mounts an RFS
  volume on the CI NVMe disk) gates the bump.
- RFS picks up new feox-nvme work: `cargo update -p feox-nvme` in RFS, in
  its own PR — fmt/clippy/tests/bare-metal gate it; the adapter's host tests
  drive the new ring code against the fake controller.
- ABI-affecting changes (anything in `feox-asi`, the `BlockDevice` trait, or
  `feox-nvme`'s public types) should land as: change + adapt in the owning
  repo, then a bump PR in the other repo the same day. The lock pins make
  the window safe.

## What each CI proves

| | Feox CI | RFS CI |
|---|---|---|
| Host | 100+ unit tests (capability table, syscall lanes, NVMe rings vs a fake controller, ABI sizes) | engine tests + crash-recovery simulation; adapter tests vs a fake controller |
| Static | x86_64 + UEFI loader builds | fmt, clippy pedantic `-D warnings`, bare-metal riscv64 build |
| Dynamic | **QEMU smoke boot**: every milestone marker M2..M23 asserted on real emulated hardware, including the RFS format/write/commit/remount/read-back | FUSE adapter build (mounts as a real Linux fs for manual/bench use) |
| Artifacts | `feox-boot-images`: QEMU ELF + Orange Pi RV/RV2 flat Images + checksums + boot guide | Debian/SUSE `rfs-fuse` packages on tag (release workflow) |

The QEMU smoke boot is the ecosystem's integration test: a red X there means
the kernel, the ABI, the NVMe rings, the adapter, or the engine regressed —
one log read localizes which.

## Long-term direction

- **`feox-asi` is the stability boundary.** Apps and the libOS speak only
  ASI; kernels and services can be reworked freely behind it. Treat additions
  as cheap and changes as expensive (size tests enforce struct layouts).
- **Release scheme**: tag both repos in lockstep when the cross-repo seam
  changes (`feox-vX.Y` / `rfs-vX.Y`), letting the locks reference tags
  instead of raw SHAs once the cadence slows. Until then, lock-pinned SHAs +
  same-day bump PRs are the contract.
- **Possible future split**: if `feox-nvme` grows independent consumers, it
  graduates to its own repo; the `[patch]` rule and git-dep shape already
  assume that day.
- **Hardware**: the per-board artifacts (see `HARDWARE_BOOT.md`) are the
  bridge to the Orange Pi RV/RV2 arc — DT-driven drivers slot in behind the
  existing capability lanes, so the QEMU-proven milestone ladder carries to
  silicon without API change.

## Local development layout

Clone side by side (`C:\Software Projects\Feox` + `...\RFS` or `~/src/...`).
For tight loops on cross-repo changes, add temporary path overrides to
Feox's `[patch]` section (`rfs-core = { path = "../RFS/crates/rfs-core" }`,
same for `rfs-feox`) — but never commit them; CI must build from the pinned
git revisions.
