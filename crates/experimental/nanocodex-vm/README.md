# Nanocodex VM

Retained libkrun workspaces and canonical workspace tools for Nanocodex.

`nanocodex-vm` is an experimental, unpublished, library-first crate. An
application owns one [`VmWorkspace`] for each isolation boundary, gives its
[`tools::VmTools`] to one or more agents, and keeps the VM alive across
sequential turns. The crate does not own agent scheduling, evaluation policy,
payment providers, or secrets.

The normal API is:

- [`VmWorkspaceBuilder`] materializes and launches a retained private
  workspace;
- [`image`] prepares immutable root images;
- [`tools`] stages the companion guest and exposes VM-backed Nanocodex tools;
  and
- [`host`] contains the lower-level libkrun, launch-record, networking, and
  egress types used by specialized applications.

The crate root intentionally re-exports only [`VmWorkspace`],
[`VmWorkspaceBuilder`], and [`VmWorkspaceError`].

## Use VM-backed workspace tools

Build the static Linux companion with `just build-vm-guest`, prepare one
read-only runtime disk, and launch a private copy of an immutable root:

```no_run
use nanocodex_vm::{
    VmWorkspaceBuilder,
    tools::GuestRuntimeDisk,
};

# async fn prepare() -> Result<(), Box<dyn std::error::Error>> {
let runtime = GuestRuntimeDisk::prepare(
    "target/aarch64-unknown-linux-musl/debug/nanocodex-vm-guest",
    ".cache/nanocodex/vm",
)?;
let workspace = VmWorkspaceBuilder::private_from(
    ".cache/nanocodex/images/project.ext4",
    ".nanocodex/sessions/018f/root.ext4",
    "nanocodex",
)?
.vmm_argument("vm-run-config")
.guest_runtime_disk(runtime.path())
.firmware_directory(".cache/libkrunfw/libkrunfw")
.guest_workspace("/app")
.launch()
.await?;

let tools = workspace.tools_builder().build()?;
// Pass `tools` to `Nanocodex::builder(...).tools(tools)`.

drop(tools);
workspace.shutdown().await?;
# Ok(())
# }
```

[`VmWorkspace::tools`] returns a clone-cheap capability suitable for
`NanocodexBuilder::tools_factory`. Every clone routes to the same retained
guest runtime, filesystem, and interactive shell sessions. The non-cloneable
workspace owner is the graceful-shutdown capability; drop agents, registries,
and cloned tool handles before calling [`VmWorkspace::shutdown`].

The default tool selection keeps web search, image generation, and
`update_plan` on the host. It replaces only `exec_command`, `write_stdin`,
`apply_patch`, and `view_image`, preserving their standard model-visible names
and schemas.

## Host, VMM, and guest ownership

The retained path has three processes:

```text
embedding application
  ├─ owns VmWorkspace, agent state, tools, policy, and egress leases
  └─ spawns a dedicated VMM process from a mode-0600 launch record
       └─ libkrun starts one Linux guest
            └─ nanocodex-vm-guest serves workspace tools over the console
```

The application process does not call libkrun after starting an async runtime.
Instead, [`host::VmProcessConfig::write_private`] writes a complete private
launch record and the dedicated VMM entry point calls
[`host::VmProcessConfig::run`] synchronously. This process boundary also keeps
the macOS hypervisor entitlement on the smallest executable and prevents guest
environment values from appearing in command-line arguments.

The shipped `nanocodex` binary provides that entry point as the hidden
`vm-run-config` command. A library consumer may provide the same small entry
point in its own executable:

```no_run
use nanocodex_vm::host::VmProcessConfig;

# fn vmm(config_path: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
VmProcessConfig::read(config_path)?.run()?;
# Ok(())
# }
```

On macOS, the VMM executable must be signed with the
`com.apple.security.hypervisor` entitlement. Ad-hoc signing is sufficient for
local development; distribution uses the application's normal signing
identity. `nanocodex-vm.entitlements` and `scripts/codesign-runner` provide the
repository build path.

Linux uses the same Rust API and needs no code signing. Running VMs requires
`/dev/kvm` and `libkrunfw.so.5`. The supported x86_64 static guest build uses a
musl 1.2.3 ABI floor because the pinned libkrun KVM path requires `statx`.

## Root and runtime disks

[`image::VmImageBuilder`] resolves a constrained Dockerfile/OCI build context
into a content-addressed immutable ext4 root. A session should attach only a
reflink or sparse copy made with
[`image::PreparedRootDisk::reflink_to`]; writable roots are session-private.

[`tools::GuestRuntimeDisk::prepare`] hashes the exact companion ELF and
atomically publishes a reusable 128 MiB ext4 disk. The runtime disk is mounted
read-only, independently from the writable project root. That keeps the guest
implementation identical across a sweep without mutating every root image.

Directory roots are a lower-level development escape hatch. They must already
contain `/usr/local/bin/nanocodex-vm-guest`, and direct virtiofs access does
not provide the same host mount-namespace isolation as a private ext4 root.

## Host/guest RPC protocol

This section is the complete current wire contract implemented by the host
session and `nanocodex-vm-guest`. The protocol is private implementation
detail: host and guest artifacts are built from the same Nanocodex revision,
and there is currently no version negotiation or cross-version compatibility
promise. Applications use the typed Rust API rather than constructing frames.

### Transport and envelope

The dedicated VMM's standard streams carry the guest's default virtio console:

- host to guest: VMM stdin;
- guest to host: VMM stdout; and
- diagnostics only: VMM stderr.

stdin and stdout are newline-delimited JSON. Each frame is one UTF-8 JSON
object followed by `\n`; readers also accept `\r\n`. The newline is not part of
the 64 MiB frame limit. Binary fields use standard padded base64. There is no
authentication, checksum, compression, streaming sub-frame, or handshake
beyond `ready`, because the transport is a private pipe to the owned VMM
process.

Every frame has the externally tagged envelope:

```json
{"kind":"ready","payload":{"id":0}}
```

`kind` is snake case. Every request carries a host-assigned `u64` `id`, and
exactly one response carries the same ID unless the request is cancelled or
the session fails. Responses may arrive in any order. The host allows at most
63 ordinary requests to await responses; the guest executes at most 64
requests concurrently, leaving capacity for control traffic.

### Requests and responses

`ready` establishes that the guest runtime is accepting work:

```json
{"kind":"ready","payload":{"id":0}}
{"kind":"ready","payload":{"id":0,"error":null}}
```

`tool` executes one canonical workspace tool:

```json
{"kind":"tool","payload":{"id":1,"tool":"exec_command","input":{"function":{"arguments":{"cmd":"pwd"}}},"context":{"model":"gpt-5.6","session_id":"session-1","call_id":"call-1","output_token_budget":10000}}}
{"kind":"tool","payload":{"id":1,"execution":{"output":"/app\n","success":true,"code_mode_value":null,"metadata":null,"process_trace":{"exit_code":0,"session_id":null,"original_token_count":null,"output_bytes":5,"wall_time_seconds":0.01}},"error":null}}
```

The normal adapter sends `exec_command`, `write_stdin`, `apply_patch`, or
`view_image`. `input` is exactly one of:

```json
{"function":{"arguments":{"cmd":"pwd"}}}
{"freeform":{"input":"*** Begin Patch\n...\n*** End Patch\n"}}
```

Function `arguments` remain opaque JSON. `context` contains `model`,
`session_id`, `call_id`, and `output_token_budget`; conversation history is
not copied into the guest context. `execution.output` is either a string or
the canonical ordered multimodal array of `input_text`, `input_image`, and
`input_audio` objects. `code_mode_value` and `metadata` are opaque JSON or
`null`. `process_trace` is `null` or contains `exit_code`, `session_id`,
`original_token_count`, `output_bytes`, and `wall_time_seconds`.

An execution with `"success":false` is a model-visible tool failure. A failure
of the RPC/tool runtime itself instead uses `"execution":null` and a non-null
`"error"`. Exactly one of `execution` and `error` is present.

The remaining control methods have these payloads:

| `kind` | Request payload after `id` | Response payload after `id` |
| --- | --- | --- |
| `write_file` | `path`, base64 `contents`, Unix `mode`, optional `modified_unix_seconds` | `error` |
| `create_directory` | `path`, Unix `mode`, optional `modified_unix_seconds` | `error` |
| `read_file` | `path` | base64 `contents` or `error` |
| `execute` | `program`, `arguments`, `current_directory`, `environment`, `timeout_millis`, `max_output_bytes` | `exit_code`, base64 `stdout`, base64 `stderr`, `error`, `timed_out`, `output_limit_exceeded` |
| `cancel` | `target_id` | `error` |
| `shutdown` | none | `error` |

Concrete examples:

```json
{"kind":"write_file","payload":{"id":2,"path":"/tmp/input","contents":"aGVsbG8K","mode":420}}
{"kind":"write_file","payload":{"id":2,"error":null}}
{"kind":"create_directory","payload":{"id":3,"path":"/tmp/results","mode":493,"modified_unix_seconds":0}}
{"kind":"create_directory","payload":{"id":3,"error":null}}
{"kind":"read_file","payload":{"id":4,"path":"/tmp/results/out.txt"}}
{"kind":"read_file","payload":{"id":4,"contents":"b2sK","error":null}}
{"kind":"execute","payload":{"id":5,"program":"/bin/sh","arguments":["-lc","printf ok"],"current_directory":"/app","environment":[["PATH","/usr/bin:/bin"]],"timeout_millis":60000,"max_output_bytes":8388608}}
{"kind":"execute","payload":{"id":5,"exit_code":0,"stdout":"b2s=","stderr":"","error":null,"timed_out":false,"output_limit_exceeded":false}}
{"kind":"cancel","payload":{"id":6,"target_id":5}}
{"kind":"cancel","payload":{"id":6,"error":null}}
{"kind":"shutdown","payload":{"id":7}}
{"kind":"shutdown","payload":{"id":7,"error":null}}
```

`write_file` creates parents and publishes through a sibling temporary file
plus rename. `read_file` accepts only regular files and caps contents at
32 MiB. `execute` clears the inherited environment, uses only the supplied
pairs, captures combined output up to the requested bound, and kills the
process group on timeout, output overflow, cancellation, or shutdown. It is a
bounded one-response operation rather than a streaming terminal; retained
interactive shells use the `exec_command`/`write_stdin` tool protocol.

Dropping a host request removes its pending response and best-effort queues a
`cancel` with a fresh ID. Cancelling an unknown or already completed target is
successful. The host does not wait for this automatically generated cancel
acknowledgement. `shutdown` stops acceptance, cancels active tool work and
shell process groups, runs `/bin/sync`, replies, and exits.

### Protocol failure

The session fails closed on malformed JSON, an unknown request/response kind,
an unknown field in a strict request payload, a frame larger than 64 MiB, a
partial frame at EOF, or reuse of an ID that is still active. Clean host EOF
cancels active work and exits the guest. A tool response that is too large is
replaced with a scoped tool RPC error when that fallback fits; an oversized
non-tool response terminates the session. A failed partial response is never
turned into a successful tool result.

## Egress and lifecycle

[`host::EgressLease`] is the provider-neutral output of application policy. It
combines network mode, guest environment, read-only mounts, public guest files,
and host-side guards that must live as long as the VM. The VM crate never
resolves secrets or chooses a payment provider. Conflicting environment or
mount claims fail closed.

The last workspace/tool capability kills the VMM child. Explicit shutdown
first rejects live sibling capabilities, then requests guest cancellation and
filesystem sync with a bounded exit wait. Timeouts and request cancellation
terminate process groups and descendants.

## Cargo features

The default `host` feature contains image preparation, libkrun lifecycle, and
VM-backed tool clients on Linux and macOS. `guest-runtime` contains only the
companion server and the canonical `nanocodex-tools` workspace runtime. The
split exists to produce a small static Linux guest ELF; it is not a second
public execution model. Normal native `nanocodex-tools` and
`nanocodex-oai-api` builds retain their complete default behavior.

See `docs/VM.md` in the repository for CLI operation, egress composition, and
build commands.
