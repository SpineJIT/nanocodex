# NanoUSD credits

Nanocodex credits are fixed-dollar packages fulfilled as NANOUSD on Tempo
mainnet. The CLI owns the headless purchase UX and `nanousd-api` owns orders,
payment verification, and idempotent token issuance.

NANOUSD is the TIP-1015 token at
`0x20C0000000000000000000008B4c619d2eedEc7A` on chain `4217`. It has six
decimals, so a $5 package fulfills `5_000_000` atomic units. Its transfer policy
makes the configured Nanocodex receiver the only ordinary payment destination.

## CLI flow

The commands use the active account in the Tempo Wallet store:

```console
nanocodex credits status
nanocodex credits buy 5
nanocodex credits buy 10 --no-open
nanocodex credits buy 25 --json --no-wait
nanocodex credits wait ord_... --order-token ...
```

In Stripe mode, `credits buy` creates an order and opens the returned hosted
Stripe Checkout URL in the system browser. `--no-open` prints the URL for SSH
and other headless environments. The CLI then polls the capability-protected
order until issuance is confirmed.

Card entry should not be implemented directly in the TUI. Hosted Checkout keeps
sensitive payment entry in Stripe's browser surface; collecting PAN/CVC data in
a terminal would move Nanocodex into a substantially larger PCI scope. Stripe
Terminal is for physical card readers, not an online terminal UI.

## Local faucet

The safe default is mock payment and mock issuance. Mock payment mode is
intentionally prevented from binding to a non-loopback interface.

```console
cargo run -p nanousd-api
NANOCODEX_CREDITS_API_URL=http://127.0.0.1:8789 \
  cargo run -p nanocodex-bin -- credits buy 5
```

To exercise real Tempo issuance while retaining free mock purchases:

```console
cargo run -p nanousd-api -- \
  --payment-mode mock \
  --issuer-mode alloy
```

The Alloy issuer loads the active account and P-256 access key from Tempo
Wallet. It adapts that key to Alloy's `NetworkWallet<TempoNetwork>`, then uses a
typed `ITIP20::mint` call and Alloy's normal provider transaction lifecycle. It
never invokes a shell or passes a private key on a command line. An explicit
`NANOUSD_WALLET_STORE` selects a different Wallet store. The key must be
authorized on-chain before the service starts, authorized to call NANOUSD, and
have enough allowance for its configured gas fee token. The issuer never
attaches a `key_authorization` payload. Use a dedicated, narrowly scoped issuer
key for a deployed service.

The default fee token is PathUSD because that is the funded token in the issuer
account. Set `NANOUSD_FEE_TOKEN` to match both the balance and fee-token
allowance of a different issuer account and key.

The provider uses Tempo expiring nonces. Alloy fills and signs the typed mint,
then the worker stores the exact signed envelope, transaction hash, and expiry
in SQLite before broadcasting. Retries rebroadcast those same bytes. If an
unbroadcast transaction expires, the worker first checks that its hash has no
receipt and only then signs a replacement with a fresh expiry.

## Stripe deployment

Set these values after creating the Stripe account resources:

```dotenv
NANOUSD_PAYMENT_MODE=stripe
NANOUSD_ISSUER_MODE=alloy
NANOUSD_PUBLIC_URL=https://credits.nanocodex.example
NANOUSD_DATABASE=/var/lib/nanousd-api/orders.sqlite3
NANOUSD_STRIPE_SECRET_KEY=sk_live_...
NANOUSD_STRIPE_WEBHOOK_SECRET=whsec_...
```

Expose the application through TLS and register this webhook destination in
Stripe:

```text
https://credits.nanocodex.example/v1/stripe/webhook
```

Subscribe to `checkout.session.completed` and
`checkout.session.async_payment_succeeded`. The implementation:

1. offers only server-defined $5, $10, $25, and $50 packages;
2. creates Checkout Sessions server-side with a Stripe idempotency key;
3. stores the order before redirecting the customer;
4. verifies the raw request body and timestamped `Stripe-Signature`;
5. retrieves the Checkout Session from Stripe and rechecks paid status, USD
   amount, currency, metadata, and session ID;
6. records Stripe event IDs and state transitions transactionally; and
7. fulfills from the durable worker, never from a browser redirect or client
   callback.

Webhook retries and distinct duplicate success events are safe even after an
order is fulfilled. The order read capability is generated randomly and only
its SHA-256 digest is stored.

Before enabling live mode, place SQLite on persistent storage with backups,
restrict filesystem access to the service account, configure Stripe webhook
alerts, and provision a dedicated Tempo issuer key. Refunds and disputes need an
explicit business policy because on-chain credits may already have been spent;
the first live version should process them manually rather than pretending a
Stripe refund can atomically claw back NANOUSD.

## HTTP surface

- `GET /health`
- `GET /v1/credits`
- `GET /v1/credits/balance/{wallet}`
- `POST /v1/credits/orders`
- `GET /v1/credits/orders/{id}` with `Authorization: Bearer <order-token>`
- `POST /v1/stripe/webhook`

The success and cancellation pages are informational only. They never change an
order's payment or fulfillment state.
