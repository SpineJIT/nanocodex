# Python binding

This Maturin/PyO3 package embeds the native Nanocodex runtime in Python. One
`Nanocodex` object owns the persistent Rust agent session, so follow-on prompts
reuse its WebSocket, retained history, response chain, and prompt-cache key.

```sh
python -m venv py/bindings/.venv
py/bindings/.venv/bin/pip install maturin
py/bindings/.venv/bin/maturin develop -m py/bindings/Cargo.toml
py/bindings/.venv/bin/python examples/python/follow_on.py
```

`prompt()` only accepts the turn and returns a `Turn`; `Turn.result()` does the
blocking wait while releasing Python's GIL. Once it completes, `Turn.usage()`
returns exact aggregate input, cache-read, cache-write, output, reasoning, and
total token counts as a dictionary. The same dictionary automatically contains
an exact USD estimate using the published `gpt-5.6-sol` rates:

```python
import os

from nanocodex import Nanocodex

agent, events = Nanocodex(os.environ["OPENAI_API_KEY"])
turn = agent.prompt("Explain the identifier req_7f3.")
turn.result()
print(turn.usage()["estimated_cost"]["usd"])
print(turn.usage()["cost_status"])
```

`cost_status` is `estimated_from_usage` or `usage_not_reported`; a missing
provider usage record is never presented as zero cost.

`AgentEvents.recv_json()` likewise releases the GIL, so applications can
consume it from a normal Python thread.
`agent.set_thinking("high")` changes the effort for subsequently accepted turns
without replacing the session. `agent.set_fast_mode(True)` similarly enables
priority service for subsequently accepted turns.

Turn control matches the native SDK surface: `turn.steer(...)` injects input at
the next safe model boundary, and `turn.cancel()` stops that exact unfinished
turn. Session branching is also exposed: `agent.spawn()` starts a clean sibling,
`agent.fork()` forks the latest safe boundary, and `agent.fork_from(turn)` forks
from a completed turn's retained checkpoint.
The Rust runtime, tools, transport, retries, history, and event ordering stay
inside the extension; no app server or per-tool Python bridge is involved.

Pass an API key positionally, or use native subscription credentials created by
`nanocodex auth login`:

```python
agent, events = Nanocodex(auth_file="/path/to/.codex/auth.json")
```

GPT-5.6 Pro is a reasoning mode, not a different model slug. Select it
independently from any supported effort level:

```python
agent, events = Nanocodex(
    api_key,
    reasoning_mode="pro",
    thinking="xhigh",  # none, low, medium, high, xhigh, or max
    fast_mode=True,
)
```

Runnable consumers live together at the repository boundary under
[`examples/python`](../../examples/python): `follow_on.py` demonstrates retained
conversation state, `events.py` consumes the ordered event receiver, and
`lifecycle.py` exercises steer, spawn, and historical fork.
