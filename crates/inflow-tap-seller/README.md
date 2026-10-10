# inflow-tap-seller

Verify signed HTTP requests using InFlow's profile of Visa Trusted Agent Protocol
(TAP). TAP identifies a request signed by a trusted agent key. It does not log in a
buyer, authorize a purchase, or verify payment. Keep application authentication,
MPP/x402 verification, and business authorization separate.

```toml
[dependencies]
inflow-tap-seller = "0.4.0"
```

This crate is independent of the InFlow payment crates. It requires no InFlow
account or API key. Its default key resolver uses Reqwest and the application's
Tokio runtime. Custom resolvers and replay stores can use other asynchronous
implementations; the verifier itself does not create a runtime.

## Verify before running application code

Create one verifier for the application's lifetime. Creating one for every request
would discard its key cache and replay history.

```rust,no_run
use inflow_tap_seller::{Request, Verifier, VerifierOptions};

async fn handle(verifier: &Verifier, request: &Request) -> Result<String, inflow_tap_seller::Error> {
    verifier.with_verified(request, |facts| async move {
        format!("Verified agent key: {}", facts.keyid)
    }).await
}

let verifier = Verifier::new(VerifierOptions::default())?;
# Ok::<(), inflow_tap_seller::Error>(())
```

Supply the original method, absolute URL, HTTP headers, and exact body bytes.
Use `Request::new` to convert raw header pairs, or construct a `Request` with your
framework's `HeaderMap`. Preserve duplicate values with `HeaderMap::append`; the verifier rejects
ambiguous signed headers. `body: None` means no body, while `Some(Vec::new())`
means an explicitly supplied empty body. Convert strings with `as_bytes()`;
do not parse and reserialize JSON before verifying its digest.

Use a configured public origin or trusted proxy configuration to construct the
URL. Do not trust arbitrary incoming `Forwarded` or `X-Forwarded-*` headers.
Keep encoded paths and the original query order. The runnable
[Axum example](../../examples/README.md#tap-seller) demonstrates this boundary.

## Keys, replay, and customization

`VerifierOptions` accepts `Arc<dyn KeyResolver>`, `Arc<dyn ReplayStore>` and an
optional clock returning Unix milliseconds, matching Node's `Date.now`. Signature
creation and expiration fields remain integer Unix seconds. Custom resolvers return trusted
32-byte Ed25519 public keys, not permission to skip signature verification.
Custom failures return `Error` and prevent application-handler invocation.

`VisaKeyResolver` retrieves `https://mcp.visa.com/.well-known/jwks`. Its configurable
defaults are a one-hour fresh cache, a 24-hour maximum age for using a matching
cached key during an outage, and a three-second retrieval deadline. Successful
refresh replaces the entire key set. Missing identifiers are remembered within
that cache generation. Redirects and automatic retries are disabled; key responses
are limited to 1 MiB. Only an application-configured URL is fetched.

Concurrent requests share a key retrieval. Dropping one caller's future does not
cancel retrieval for other callers. Unlike Node's running Promise, the Rust shared
future pauses if no caller polls it; another caller resumes it, or dropping the
resolver releases it. It does not spawn a detached background task. The retrieval
deadline still applies when the future resumes.

The default `MemoryReplayStore` atomically claims `(keyid, nonce)` until signature
expiration. It protects one process only. Use a shared atomic store for multiple
workers or instances. Retain one store for the application lifetime; this is not a
payment ledger or durable payment idempotency mechanism.

## Profile and results

The supported profile is one `sig2` Ed25519 signature covering method, authority,
encoded path and query. Body-bearing requests also cover `content-type` and a
SHA-256 `content-digest`. Validity is checked once at verification start, allowing
at most eight minutes. Key retrieval may finish after expiration.

`verify` returns `VerifiedFacts`: key identifier, normalized `ed25519` algorithm,
`Intent::Browse` or `Intent::Pay`,
nonce, creation/expiration seconds and covered components in signed order. Only
Ed25519 is accepted. An intent is the agent's signed assertion, not buyer consent.
`with_verified` calls its handler only after cryptographic verification and a
successful replay claim. Handler results are returned unchanged, including an
application's own `Result`; the application chooses HTTP responses and logging.

`Error.code` distinguishes malformed inputs, digest mismatch, invalid lifetime,
not-yet-valid/expired signatures, missing keys, retrieval failure, invalid
signatures and nonce replay. The shared
[TAP contract](https://github.com/inflowpayai/inflow-specs/blob/main/contracts/tap.md)
lists the stable failure codes and the intentionally restricted profile.
