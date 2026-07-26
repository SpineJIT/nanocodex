# MPP Charge integration

## Boundary

MPP remains an application concern. `bin/nanocodex` composes the private
`mpp-egress` support crate with the normal Nanocodex HTTPS Responses client.
No public Nanocodex library crate contains wallet or payment behavior.

The Tempo provider supports one payment path: estimated, up-front
`tempo/charge` over HTTPS. It deliberately does not configure an MPP
WebSocket/session transport.

```text
Nanocodex Responses HTTPS client
              |
     loopback MPP egress
              |
       HTTP 402 challenge
              |
   MPP TempoAccountsProvider
              |
 tempo-alloy Accounts wallet
              |
       NanoUSD payment
              |
     exact request replay
              |
      streamed SSE response
```

The proxy, not Nanocodex, owns the upstream OpenAI credential. Nanocodex sends
the Responses request, `Accept-Payment: tempo/charge`, and an optional
deployment access key.

## Accounts and signing

The CLI opens the Tempo Accounts SDK store at
`~/.tempo/wallet/store.json` by default. `tempo-alloy` owns the concrete
`TempoAccountsWallet` and lazily selects an authorized access key when a
payment transaction is prepared. The resulting `TempoAccessKey` pins that
exact key through authorization resolution, gas filling, sponsorship, and
signing.

There is no Nanocodex wallet wrapper, signer enum, or signing-mode flag.
`tempo-alloy` implements Alloy's existing wallet and filler traits;
`mpp-rs` owns Challenge decoding, Charge transaction construction, optional
fee sponsorship, and receipt lifecycle; Nanocodex only supplies configuration.

NanoUSD on Tempo mainnet is the configured payment input. A service that
charges NanoUSD is paid directly. For another supported stablecoin challenge,
MPP may use the Stablecoin DEX to swap from NanoUSD within the configured
slippage bound.

## CLI

Enable paid Responses and paid HTTP tool egress with:

```text
nanocodex run "say hello" --provider.tempo
nanocodex --provider.tempo --prompt "say hello"
```

Relevant global options:

```text
--provider.openai
--provider.tempo
--provider.tempo.api-base-url <https-url>  # default https://openai.mpp.tempo.xyz/v1
--provider.tempo.wallet-store <path>       # default ~/.tempo/wallet/store.json
--provider.tempo.rpc-url <url>             # default Tempo mainnet RPC
--provider.tempo.swap-slippage-bps <bps>   # default 100
--provider.tempo.egress-max-charge <units> # default 100000 NanoUSD atomic units
--provider.tempo.api-key <key>             # optional gated deployment key
```

Tempo selects the HTTPS Responses transport. Explicitly selecting WebSocket
with `--provider.tempo` is rejected during startup. Direct OpenAI continues to
default to its persistent Responses WebSocket.

The API base must use HTTPS. Plain HTTP is accepted only for loopback
development endpoints, including an SSH-forwarded service.

## HTTP tool egress

`--provider.tempo` also starts a private HTTP forward proxy on an ephemeral
loopback port. Nanocodex routes its own Responses and remote-tool clients
through that proxy and gives authenticated proxy environment variables plus an
ephemeral CA to workspace-tool child processes. It does not mutate the parent
environment or MCP transports.

The egress proxy buffers a request body up to 16 MiB so it can replay the exact
request after a valid 402 challenge. It rejects redirects, protocol upgrades,
unsupported payment methods, malformed challenges, and charges above
`--provider.tempo.egress-max-charge`. The wallet and signing key never leave
the Nanocodex process.

MPP tracing records full request, response, challenge, and credential content.
Operators must protect those traces like wallet and conversation data.

## Credits onramp

`nanocodex credits` and `nanousd-api` are separate from MPP settlement:

1. the CLI reads the same Accounts SDK wallet and requests a credit package;
2. Stripe Checkout and its signed webhook establish successful payment;
3. `nanousd-api` issues the package's NanoUSD to the account on Tempo mainnet;
4. later MPP Charge requests spend those NanoUSD credits.

The service binds to `127.0.0.1:8789` by default. During development it can stay
private behind an SSH tunnel; a later Tailscale Funnel can expose the same
HTTP service after both host and SentinelOne firewall rules are approved.

## Validation

Before merging:

- run rustfmt, Clippy with warnings denied, and the focused Nanocodex tests;
- run MPP's minimal `client,tempo` build and sponsored Charge tests;
- complete a live HTTPS Responses request whose 402 challenge names NanoUSD;
- retain the MPP receipt and verify the wallet's NanoUSD debit;
- create a Stripe Checkout order, deliver its signed webhook, and verify the
  corresponding NanoUSD issuance transaction.
