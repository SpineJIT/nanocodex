# React + Vite Worker example

This app embeds the browser build of `nanocodex-wasm` in a module Worker. The
Worker owns one persistent `Nanocodex` session, forwards its ordered events to
React, and registers the browser-native `browserInfo` tool.

```sh
just bootstrap-bindings
just build-react-example
just dev-react-example
```

`just dev-react-example` starts the Vite app with the existing OpenAI API-key
path selected by default. That path uses the Cloudflare Worker upgrade proxy;
the key never enters the page or browser Worker.

Selecting **Tempo MPP** opts into the keyless payment path. Only then does the
page dynamically load the Tempo Accounts SDK and show Tempo-specific UI. The
page authorizes a scoped access key, then the dedicated Worker dynamically loads
the MPP integration from same-origin IndexedDB and opens
`wss://openai.mpp.tempo.xyz/v1/responses`. The normal OpenAI bundle and runtime
do not initialize a wallet, access key, or payment session.

In either mode the Worker owns one persistent Nanocodex agent. In MPP mode it
also owns one persistent MPP manager, which reuses its paid channel. Serialized
channel state is retained in IndexedDB and can be recovered after a Worker or
page restart. Tempo wallet, signer, channel, and payment diagnostics are rendered
only while MPP is selected; raw ordered agent events are available in both modes.
