# inflow-x402-buyer

Pay x402 V2 resources using an InFlow-managed wallet, or compose external-wallet
signers through the upstream `x402-types` interfaces.

```toml
[dependencies]
inflow-x402-buyer = "0.3.0"
reqwest = { version = "0.13", default-features = false, features = ["rustls"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## InFlow-managed payments

For a runnable Buyer and matching Seller, follow the
[Sandbox example walkthrough](https://github.com/inflowpayai/inflow-rust/tree/main/examples#x402).

Create an InFlow account and API key in [Sandbox](https://sandbox.inflowpay.ai) for
testing or [production](https://app.inflowpay.ai) for live payments. Buyer endpoints
accept authenticated accounts; a Seller account can also act as a Buyer. Keep the
API key on the InFlow platform client, not on requests sent to merchants.

```rust,no_run
use inflow_x402_buyer::{
    Authentication, Buyer, BuyerOptions, CancellationToken, ClientOptions,
    Environment, HttpBuyer, SignOptions, WaitOptions,
};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let cancellation = CancellationToken::new();
let buyer = Buyer::new(BuyerOptions {
    client: ClientOptions {
        authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
        environment: Environment::Sandbox,
        ..Default::default()
    },
    ..Default::default()
}, &cancellation).await?;

let payments = HttpBuyer::new(Some(buyer))?;
let request = reqwest::Client::new()
    .get("https://merchant.example/report")
    .build()?;
let response = payments.execute(
    request, SignOptions::default(), WaitOptions::default(), &cancellation,
).await?;
// Inspect the merchant's status and body. A paid retry can still fail to deliver the resource.
assert!(response.status().is_success());
# Ok(())
# }
```

Construction fetches `/v1/transactions/x402-supported`, the Buyer capability endpoint.
The capability snapshot lasts one hour; `refresh_supported` explicitly refreshes it.
Balances are fetched fresh when selection must choose among several balance offers.
The default scheme order is `balance`, then `exact`. Within the preferred scheme,
selection favors an asset the Buyer can afford. If balances cannot be read or none
cover the price, selection retains the first preferred offer; the platform decides
whether payment is authorized. Permit2 offers are not routed to InFlow-managed signing.

For linked-card payments, include `"instrument"` in `BuyerOptions::prefer` and set
`instrument_id: Some(card_id)` to select a particular card. With `instrument_id: None`,
the platform uses the Buyer's primary card. This selection is sent only for the
instrument scheme; it does not change the Seller's payment requirements or the
default `balance`, `exact` preference. A rejected card returns the platform error
without trying another card or creating a replacement payment. An explicit typed
selection takes precedence over `SignOptions::transaction_fields["instrumentId"]`.

## Show the approval before waiting

Call `Buyer::select` with a decoded upstream `PaymentRequired<OriginalJson>`, then
pass the chosen offer to `Buyer::prepare`. The returned `Payment` exposes
`approval_id`, `transaction_id`, `status`, `cancel`, and `wait`.

`wait` consumes the handle and returns the server's encoded payment unchanged.
Use `encoded_payload` as the `Payment-Signature` header if sending the paid request
yourself. Its default wait budget is 15 minutes, with five-second polling. Temporary
network, rate-limit, and server failures during polling are retried within that budget;
creation is never automatically retried. `SignOptions::payment_id` lets you supply
a stable identifier for one payment. Reuse it only for that same payment.

Cancel the token returned by `Payment::cancellation` to interrupt a pending wait.
`cancel().await` stops the operation and waits for one coalesced approval-cancellation
attempt with its own five-second budget. Dropping a pending handle or wait future
schedules best-effort cleanup on the caller's Tokio runtime. Process or runtime exit
can prevent that cleanup; explicit cancellation is the dependable way to await it.
Without a creation response, the SDK cannot know which approval to cancel.
Completion disarms cleanup before resource delivery: a failed merchant request does
not reverse a signed payment.

## Read settlement status

Call `buyer.get_payment_status(transaction_id, PaymentStatusOptions::default(), &token)`
to read an existing payment, including a pending instrument payment. The returned
JSON preserves the server's `status`, transaction identifier and optional `nextAction`.
An `authenticate_card` action supplies a dashboard URL; the SDK does not open it.
Payload readiness and absence of an action do not establish settlement.

Each call fetches a fresh snapshot with no retries by default. Set `retries` to
permit up to three additional read attempts. Cancellation stops only the read.
No status outcome creates or cancels a payment, changes the selected card, or
replaces its signed payload. A failed read or 404 does not establish that the
original payment failed. Keep its identifier and payment material for recovery.

## External wallets and upstream behavior

`HttpBuyer::new(None)` supports external-wallet-only use without an InFlow account
or platform capability lookup. Register an upstream `X402SchemeClient` with
`register`; `with_selector` chooses among external-wallet candidates. With both
wallet types configured, a supported InFlow offer takes priority. Failure of that
payment does not silently switch to an external wallet and create another payment.

Enable `features = ["evm"]` to use the re-exported `evm::V2Eip155ExactClient`
with an upstream `SignerLike` (including Alloy's `PrivateKeySigner`). Enable
`features = ["solana"]` for `solana::V2SolanaExactClient`, which takes a Solana
signer and upstream `RpcClientLike`. Register these clients with `HttpBuyer::register`.
Neither chain dependency is enabled by default. Wallet secrets and blockchain
connections remain owned by the integrator; the SDK does not create or fund wallets.

Unlike upstream's synchronous selector, InFlow selection can await a fresh balance
lookup before signing. Each request has its own selection state; requests do not
share a mutable selected offer. External signers retain upstream selection behavior.

Automatic HTTP payment requires a replayable request body. The adapter rejects
streaming bodies before sending the request or creating an approval; it does not
buffer arbitrary streams. Upstream currently discovers an uncloneable request only
after signing ([issue #132](https://github.com/x402-rs/x402-rs/issues/132)). For a
streaming integration, use explicit payment preparation and control transmission
yourself. The adapter sends at most one paid retry and follows no redirects, so it
does not forward payment or service-authentication credentials to another destination.
It also does not replace an existing payment header after another 402 response.

### Exact Permit2 allowance sponsorship

The upstream Rust `exact` signer in `x402-chain-eip155` 2.0.2 does not create an
EIP-2612 allowance permit when the merchant advertises `eip2612GasSponsoring`.
It signs the Permit2 payment, but copies the unsigned sponsorship declaration.
The InFlow adapter omits that unsigned declaration from exact payments. A signed
permit produced by a custom signer is retained. An advertisement is not a permit
and must not be treated as proof of sponsorship.
Node's exact signer can create the allowance permit when the token and signer
support it and allowance is insufficient. This parity gap is tracked in
[upstream issue #133](https://github.com/x402-rs/x402-rs/issues/133).

Ordinary Permit2 payments require sufficient existing token allowance. Arrange that
allowance separately before using the upstream exact signer. This limitation does
not mean all Permit2 payments are unsupported, and it does not apply to the separate
EIP-2612 implementation in Rust's `upto` signer. EIP-7702 sponsorship is a different
mechanism; an EIP-2612 declaration does not activate it.

The upstream exact client also requires `extra.name` and `extra.version` for
Permit2 offers. An offer without those token metadata fields produces no upstream
signing candidate, even though Node can sign that Permit2 offer. The adapter does
not invent token metadata to bypass this requirement.

### Opt-in EIP-7702 sponsorship

With the `evm` feature, attach `eip7702::Sponsorship` through
`HttpBuyer::with_extension`. Supply platform `ClientOptions`, your implementation
of `eip7702::Signer`, and an implementation of `DelegationConsent`. The signer
reads the ERC-20 allowance and signs personal messages and delegation
authorizations. The consent callback must obtain the wallet owner's permission:
delegation persists even if the subsequent payment fails.

The extension runs only for an advertised `inflowEip7702GasSponsoring` exact
Permit2 payment with insufficient allowance. It sends one preparation request to
the configured InFlow environment, never to a merchant-selected URL. Before
requesting signatures, it checks the recipient, token, amount, chain, canonical
approval-and-settlement calls, EntryPoint, delegation contract, operation hash,
and expiry. It verifies that returned signatures belong to the configured wallet.
It never broadcasts a transaction. Hosted-wallet payments do not use this extension.

See the [workspace documentation](https://github.com/inflowpayai/inflow-rust)
for the crate layout, environments, and repository verification commands.
