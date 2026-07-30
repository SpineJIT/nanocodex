# Nanocodex examples

All language consumers live at this repository boundary:

- Rust: `minimal.rs`, `follow_on.rs`, `lifecycle.rs`, `custom_tool.rs`, `subagents.rs`,
  `resume.rs`, `fork_conversations.rs`, `fork_checkpoint_bench.rs`, and `mcp.rs` are binaries
  in the `nanocodex-examples` package.
- Python: `python/` uses the native PyO3 binding (`follow_on.py`, `events.py`,
  `lifecycle.py`).
- Node.js: `node/` uses the shared Rust/WASM package with a Node WebSocket host.
- Browser: `react-vite/` runs that WASM agent in a module Worker and renders its
  ordered events in React.
- Browser CDN: `browser-cdn/` is one static HTML file that imports the published
  package directly, with no install or build step.

From the repository root:

```sh
cargo run -p nanocodex-examples --bin minimal
cargo run -p nanocodex-examples --bin lifecycle
cargo run -p nanocodex-examples --bin fork-conversations
cargo run -p nanocodex-examples --bin subagents
cargo run -p nanocodex-examples --bin subagents -- \
  "Review the retry policy using whatever clean or context-bearing workers you need"
NANOCODEX_SUBAGENT_JSONL=1 cargo run -p nanocodex-examples --bin subagents
cargo run -p nanocodex-examples --bin mcp
# stdin/stdout are raw 24 kHz mono signed-16-bit little-endian PCM:
cargo run -p nanocodex-examples --bin realtime-pipe < microphone.pcm > speaker.pcm
just build-vm-example
target/debug/vm-tools ROOTFS [GUEST_RUNTIME_BINARY_OR_EXT4]
just smoke-python
just smoke-wasm-node
just build-react-example
```

The command-line examples use `OPENAI_API_KEY` by default. The Nanocodex TUI's
`/voice [voice]` command reuses its managed ChatGPT subscription authentication
(`/voice list` shows the Codex voice set), and
`realtime-pipe` can do the same when `NANOCODEX_AUTH_FILE` points at a Codex
credential file. `realtime-pipe` demonstrates the device-neutral voice API and
delegates spoken coding requests to the same retained `Nanocodex` agent handle.
The browser example instead asks the
embedding application for an already-authorized Responses WebSocket URL;
standard browser WebSockets cannot attach the upgrade authorization header.

`vm-tools` does not call the model. It proves all VM-backed standard workspace
tools against one retained guest and accepts either a directory root containing
`/usr/local/bin/nanocodex-vm-guest` or an ext4 root plus a guest-runtime ELF or
read-only runtime image. A runtime ELF is packed into a temporary ext4 image,
and a supplied ext4 root is reflinked or sparse-copied into a private per-run
disk before boot. On macOS, `just build-vm-example` also applies the required
Hypervisor entitlement.

`subagents` exposes generic `spawn_agent`, `fork_agent`, and `prompt_agent` Code
Mode tools; its Rust host contains no worker graph. The parent model decides the
orchestration topology and follow-ups from the goal. Initial workers return an
`agent_id` with their attributed report; `prompt_agent` sends later turns
through that child's retained session. `tools_factory` reinstantiates
agent-relative handlers with a weak `AgentHandle` for every driver. Its
`spawn()` method reuses private builder configuration without inheriting
conversation history, while `fork()` targets the agent that actually invoked
the tool.
The example prints only the final root answer by default. Set
`NANOCODEX_SUBAGENT_JSONL=1` to emit each child's lifecycle JSONL to stderr;
the records retain their native request IDs and sequence numbers without a
custom merged-event protocol.

The MCP example defaults to the public OpenAI documentation MCP. Override
`NANOCODEX_MCP_URL` for another Streamable HTTP server and set
`NANOCODEX_MCP_BEARER_TOKEN` when it requires bearer authentication.
