# nanocodex-tui

Framework-independent transcript state and event reduction for Nanocodex TUI
renderers. It has no React, DOM, transport, or styling dependency.

It also owns the typed TUI controller protocol (`TuiCommand`, `TuiMessage`, and
`TuiTarget`) shared by a renderer and its agent Worker.

```ts
import {
  applyAgentEvents,
  initialTerminalState,
  queuePrompt,
} from "nanocodex-tui";

let transcript = queuePrompt(
  initialTerminalState(),
  1,
  "Explain how retained context works.",
);
transcript = applyAgentEvents(transcript, [{
  protocol_version: 1,
  request_id: "session-019c",
  seq: 1,
  type: "run.started",
  payload: {},
}]);
```

Most applications should use `nanocodex-tui-react`. Use this package directly
when implementing a renderer for another UI framework.
