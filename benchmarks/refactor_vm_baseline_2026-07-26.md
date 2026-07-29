# VM and image baseline — 2026-07-26

This is the first reproducible baseline for the refactor's VM slice. It keeps
image-cache work, protocol overhead, and actual libkrun lifecycle time separate
so cold compilation or image construction is never reported as agent latency.

## Environment

- MacBookPro18,2, Apple M1 Max, 32 GiB
- macOS 26.3.1 (25D771280a)
- rustc 1.97.1 (`8bab26f4f`, LLVM 22.1.6)
- Cargo `bench` profile, optimized, debug information disabled
- Criterion 0.7, 20 samples for deterministic benchmarks and 10 for live VM
- arm64 Alpine 3.24 immutable root disk, 512 MiB logical size
- current `aarch64-unknown-linux-musl` `nanocodex-vm-guest`
- libkrun firmware 5

The source worktree was based on `refactor/06-vm@5535935`; the measurements
include the uncommitted VM/image slice subsequently recorded by this document.

## Results

Criterion's reported interval is shown verbatim. It is a confidence interval
for the estimate, not a percentile claim.

| Operation | Fixture | Estimate |
| --- | --- | --- |
| warm guest-runtime prepare | path-scoped source record and prepared 128 MiB ext4 disk | 32.662–38.613 µs |
| warm image prepare | local immutable OCI reference and prepared 512 MiB ext4 disk | 62.020–63.628 µs |
| attempt root reflink | prepared 512 MiB ext4 disk to a fresh APFS path | 114.22–119.44 µs |
| retained protocol RPC | typed command over a retained local process protocol | 145.61–147.85 µs |
| protocol spawn + first RPC + shutdown | local protocol process, no VM | 3.5824–3.8843 ms |
| retained live-VM command RPC | `/bin/true` in one already-running libkrun guest | 320.93–352.97 µs |
| live VM boot + first RPC + shutdown | private root reflink, current guest runtime, 2 vCPU/768 MiB | 162.63–165.22 ms |

Reproduce the deterministic measurements with:

```sh
just bench-vm
```

Reproduce the live lifecycle measurement after building the signed VMM and
guest:

```sh
just build-vm-guest
just bench-vm-live \
  .cache/vm/images/226b091c02ae88b383c42617e8508d4e72588b8cadaa8b75113548d2ee205336.ext4 \
  target/aarch64-unknown-linux-musl/debug/nanocodex-vm-guest \
  .cache/libkrunfw/libkrunfw
```

`NANOCODEX_VM_RUNTIME` accepts either the guest ELF or a prepared runtime ext4
disk. The public `GuestRuntimeDisk::prepare` path stages an ELF once before
Criterion starts timing and reuses the content-addressed runtime identity.

## Regression gates

The following machine-local budgets apply to this M1 Max baseline. CI keeps the
structural gates deterministic; numeric comparisons run on a pinned benchmark
host because filesystem and hypervisor timings are machine-sensitive.

| Operation | Budget |
| --- | --- |
| warm guest-runtime prepare | ≤ 100 µs |
| warm image prepare | ≤ 100 µs |
| attempt root reflink | ≤ 200 µs |
| retained protocol RPC | ≤ 250 µs |
| retained live-VM command RPC | ≤ 600 µs |
| live VM boot + first RPC + shutdown | ≤ 250 ms |

Hard structural gates:

- a healthy guest-runtime hit reads only its source/disk metadata and atomic
  record; source or disk changes trigger complete byte/ext4 validation;
- a valid warm prepared-disk hit does not decode OCI layer contents or launch a
  VM;
- two concurrent preparations of one cache key publish exactly one disk and
  report one `created` plus one `hit`;
- different image references and OCI layers resolve as sibling futures, with
  each boundary capped at eight concurrent operations;
- cache records and disks publish atomically under per-artifact process locks;
- every attempt mutates a reflink/sparse copy, never the immutable cache disk;
- one root-agent tree shares one retained VM and retained guest shell sessions;
- request cancellation is targeted and does not stop sibling guest work;
- output, protocol frames, file reads, and shutdown are bounded;
- dropping the final capability kills the VMM and releases egress guards; and
- one bounded `vm.image.prepare` root contains its resolve/format/build children
  and preserves the complete Dockerfile as a span event.

The normal agent path pays neither image construction nor VM boot per tool
call. The measured retained live-VM boundary is sub-millisecond, leaving model,
network, and actual tool work as the intended critical-path costs.

The final warm rerun followed the public-API and cache-integrity audit. An
intermediate implementation opened ext4 on every healthy hit and measured
112.39–114.81 µs, breaching the 100 µs gate. The retained design validates the
existing size/mtime cache record on healthy hits, opens ext4 when that record is
missing or changed, and atomically rebuilds an invalid disk. That restored the
warm path to 62.020–63.628 µs while preserving the new corruption-repair test.
An earlier reflink sample taken with the filesystem nearly full measured
151.99–157.01 µs; after removing only reproducible development artifacts and
restoring free space, it returned to 112.28–131.16 µs and the final rerun above.

## Feature-parity evidence

- Existing host `exec_command`, `write_stdin`, `apply_patch`, and `view_image`
  remain the default and retain their schemas.
- VM-backed implementations are selected through the same `Tools` registry and
  preserve retained shell behavior and multimodal output.
- The real smoke booted libkrun and exercised all four tools through one current
  guest before graceful shutdown.
- The ignored live image integration test booted the same signed VMM through
  `VmImageBuilder`, ran `RUN printf nanocodex-vm-image-live >
  /nanocodex-vm-image-proof`, and read the exact bytes back from the published
  ext4.
- The guest-only build excludes OAI transport, TLS, Code Mode, and MCP
  implementation dependencies; the normal native `nanocodex-tools` default
  still includes MCP, `tool_search`, Code Mode, image processing, and remote
  tools.
- The VM OCI/Dockerfile cache identity is versioned independently from
  higher-level consumers.
