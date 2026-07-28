# nanocodex-tui-react

The canonical browser rendering of the Nanocodex TUI. It preserves the native
interaction model—streaming, steering, queueing, cancellation, history edits,
branches, `/btw`, readline keys, image paste, and scroll-follow behavior—while
letting the embedding application own its visual theme.

```tsx
import {
  createConfig,
  NanocodexProvider,
} from "nanocodex-react";
import { NanocodexTui } from "nanocodex-tui-react";
import "nanocodex-tui-react/structure.css";
import "nanocodex-tui-react/theme.css"; // optional default theme

const config = createConfig({
  worker: () => new Worker(
    new URL("./agent.worker.ts", import.meta.url),
    { type: "module" },
  ),
  reasoningMode: "standard",
  thinking: "high",
});

<NanocodexProvider config={config}>
  <NanocodexTui className="my-agent" />
</NanocodexProvider>
```

`structure.css` is the small correctness layer for scrolling, virtualization,
composer layout, and responsive split panes. `theme.css` is optional. Override
its semantic `--nc-*` custom properties, or omit it and target the stable
`data-nc-part`, `data-kind`, `data-state`, and `data-*` state attributes with
plain CSS, Tailwind, CSS Modules, CSS-in-JS, or another styling system.

The component fills its parent. Width constraints belong to the embedding app.
