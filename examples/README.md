# Run a Sandbox payment

These four programs connect to **InFlow Sandbox**. The Sellers run on your computer;
configuration, approval, and payment use Sandbox accounts. They are not simulated
payments. Run the commands from the repository root with Rust 1.93 or newer.

| Program       | Source                          | What it demonstrates                                      |
| ------------- | ------------------------------- | --------------------------------------------------------- |
| `mpp-seller`  | [Handler](src/mpp_seller.rs)    | Signed offers, validation, and settlement before delivery |
| `mpp-buyer`   | [Flow](src/mpp_buyer.rs)        | Charge selection, approval waiting, and credential replay |
| `x402-seller` | [Routes](src/x402_seller.rs)    | Protected Axum routes using upstream payment middleware   |
| `x402-buyer`  | [Flow](src/x402_buyer.rs)       | Offer selection, approval, and one paid request           |

The short programs in [`src/bin`](src/bin) read configuration and start the
application. The linked files contain the integration steps. This package is not
published; its server and terminal-signal dependencies stay outside the SDK crates.

## Prepare your accounts

1. Register at [InFlow Sandbox](https://sandbox.inflowpay.ai). Accepting payments
   requires a **Seller** account and its dashboard API key. A Developer account
   cannot authorize Seller configuration.
2. Use a separate Buyer account and key to make the two roles easy to follow.
   Developer accounts can buy; Seller accounts can also buy.
3. For balance-funded payments, fund the Buyer's Sandbox USDC balance with test
   assets using the dashboard's deposit flow. An API key does not provide funds
   or guarantee approval. Do not deposit mainnet funds into a test environment.
4. Build all four programs:

   ```sh
   cargo build --locked -p inflow-examples --bins
   ```

These examples explicitly select `Environment::Sandbox`; the SDK itself defaults
to production. They read exported variables, not `.env` files. Never commit keys.
Keep [Sandbox Approvals](https://sandbox.inflowpay.ai/approvals/) open while paying.

## MPP

In the Seller terminal, generate a private challenge-signing secret once. This is
separate from the API key. Share it across instances accepting the same challenges;
changing it invalidates outstanding challenges.

```sh
export INFLOW_API_KEY='your-sandbox-seller-key'
export MPP_SECRET_KEY="$(openssl rand -hex 32)"
cargo run --locked -p inflow-examples --bin mpp-seller
```

The Seller listens on `127.0.0.1:3000`. Check both routes without paying:

```sh
curl -i http://127.0.0.1:3000/free
curl -i http://127.0.0.1:3000/api/widgets
```

`/free` returns HTTP 200 and `{"ok":true}`. `/api/widgets` returns HTTP 402 with
a `WWW-Authenticate: Payment ...` challenge for **0.01 USDC** on the balance rail.

In a separate Buyer terminal:

```sh
export INFLOW_API_KEY='your-sandbox-buyer-key'
cargo run --locked -p inflow-examples --bin mpp-buyer
```

The Buyer prints the selected charge terms and approval identifier. Approve in
Sandbox if requested; an account policy can allow immediate approval. The program
then sends the full credential to the same resource once. Success prints HTTP 200,
`{"widgets":[1,2,3]}`, and the receipt method and reference. It does not print keys
or payment credentials.

This example selects the first advertised **InFlow charge**, not Tempo or a
subscription. Running against another `TARGET_URL` requests that Service's offered
charge; it is not an interactive price-confirmation interface. Apply your own
spending policy before calling `prepare` in an application.

The direct handler calls `Offer::accept`, which validates and settles before
returning a receipt. Only then does it return widgets. A failure to confirm payment
returns HTTP 502 without a fresh challenge or paid resource. Malformed Payment
authorization returns HTTP 400. Check transactions before retrying an uncertain
settlement: an HTTP failure does not prove that no payment occurred.

## x402

In the Seller terminal:

```sh
export INFLOW_API_KEY='your-sandbox-seller-key'
cargo run --locked -p inflow-examples --bin x402-seller
```

The Seller listens on `127.0.0.1:3001` and constructs **0.01 USDC** offers from its
configured methods and wallets. It stops at startup if no protected route can be
constructed from that configuration.

```sh
curl -i http://127.0.0.1:3001/free
curl -i http://127.0.0.1:3001/api/widgets
```

`/free` returns HTTP 200. `/api/widgets` returns HTTP 402 with `Payment-Required`
listing the offers. Run the Buyer in its separate terminal:

```sh
export INFLOW_API_KEY='your-sandbox-buyer-key'
cargo run --locked -p inflow-examples --bin x402-buyer
```

The SDK selects a supported InFlow-managed offer, preferring balance before exact.
The example prints its terms and approval identifier, waits for approval, and sends
one signed payment. Success prints HTTP 200, the widgets, and the settlement
receipt's network and transaction. The platform API key is not sent to the Seller.

The Axum adapter verifies payment, runs the handler, and settles before releasing
its successful response. The handler only constructs JSON. Do not put irreversible
fulfillment in it without separately reconciling payment outcomes: middleware does
not make database writes atomic with settlement. Upstream settlement failure returns
HTTP 402 without a success receipt. The Buyer stops instead of paying again.

## Cancellation and failures

Press Ctrl-C while a Buyer waits. It cancels the operation and keeps the runtime
alive for known pending-approval cleanup, bounded by the SDK's independent
five-second cleanup limit. If creation was interrupted before an approval identifier
arrived, there is no known approval to cancel. Cancellation never reverses a payment.
Sellers use graceful shutdown to let in-flight handlers finish.

Both Buyers refuse redirects and make at most one paid request. A second 402,
malformed response, failed receipt, missing receipt after payment, or other HTTP
failure exits unsuccessfully. Inspect Sandbox transactions before retrying. Receipt
decoding is not independent proof of settlement.

Merchant requests have a 30-second timeout and resource bodies a 1 MiB limit.
Approval waits use the SDK's 15-minute default budget.

## Settings and adaptation

| Variable         | Used by    | Meaning                                               |
| ---------------- | ---------- | ----------------------------------------------------- |
| `INFLOW_API_KEY` | All        | Sandbox API key for the corresponding account         |
| `MPP_SECRET_KEY` | MPP Seller | Private challenge-signing secret of at least 32 bytes |
| `TARGET_URL`     | Buyers     | Resource URL; defaults to the matching local Seller   |

Set `TARGET_URL` to the Seller's `/free` route to check an unpaid response. The
program reports that it initiated no payment. Unset it to return to the paid route.
The examples have no platform URL override or production switch.

The Sellers bind only to loopback and have **no application login**. For an
authenticated MPP route, authenticate first and set `requires_auth: true` so payment
uses `Payment-Authorization`, leaving `Authorization` for Service authentication.
Preserve that authentication on the Buyer retry. For x402, retain it alongside
`Payment-Signature`. Do not use the InFlow API key as a Service token.

For integrations beyond these charge examples:

- [MPP Buyer subscriptions, cancellation, and caller-owned MCP transport](../crates/inflow-mpp-buyer/README.md).
- [MPP Seller body binding, Tempo terms, and upstream limitations](../crates/inflow-mpp-seller/README.md).
- [x402 external wallets and optional EIP-7702 sponsorship](../crates/inflow-x402-buyer/README.md).
- [x402 configuration and explicit measured settlement](../crates/inflow-x402-seller/README.md).
- [Axum settlement ordering and automatic-metering limitation](../crates/inflow-x402-axum/README.md).

Use HTTPS when adapting these programs to a remote Service. The example package
uses local path dependencies. In another application, depend
on the relevant published SDK crates, not `inflow-examples`.

## TAP Seller

This independent example verifies agent-signed requests without payments or an
InFlow API key. Run it from the repository root:

```sh
PUBLIC_ORIGIN=http://127.0.0.1:3002 cargo run --locked -p inflow-examples --bin tap-seller
```

The server binds to `127.0.0.1:3002` and accepts GET/POST at `/api/catalog`.
An unsigned request is deliberately rejected:

```sh
curl -i http://127.0.0.1:3002/api/catalog
```

Expect HTTP 401. To receive the catalog, a TAP agent must sign the request for the
configured `PUBLIC_ORIGIN` using a private key whose public key is available from
the configured trusted resolver. The example uses Visa's public key endpoint;
an arbitrary locally generated key will not pass. The automated HTTP example test
supplies a synthetic trusted resolver and signs real requests; it tests successful
access and nonce replay without contacting Visa.

For deployment behind a proxy, set `PUBLIC_ORIGIN` to the externally signed
origin. The route ignores forwarding headers, preserves the encoded path and
query, and verifies body bytes before application code runs. POST bodies require
signed content-type and SHA-256 content-digest fields; bodies are limited to 1 MiB.
One application-lifetime verifier retains key cache and replay state. Replaying a
signed request returns 401. Stop with Ctrl+C.

TAP recognition is not buyer authentication or payment authorization. The sample
catalog response illustrates the protected callback; add account and payment
checks separately when your application requires them. See the
[crate guide](../crates/inflow-tap-seller/README.md) for custom key resolvers,
distributed replay stores, and cancellation behavior.

## Automated verification

`make verify` compiles and checks these programs. Tests exercise the public SDKs
and actual Axum routes over local HTTP: success, rejected validation, failed
settlement, malformed requests, cancellation, redirects, and receipt handling.
Only platform responses are scripted. These tests do not prove deployed settlement;
use the two-terminal walkthrough to verify your Sandbox accounts and configuration.

The workspace coverage tool excludes `examples/` by default; its SDK coverage
percentages do not measure these applications. The example integration and program
startup tests run as part of the workspace test and lint gates. Packaging checks
exclude this unpublished package while continuing to verify every SDK crate.
