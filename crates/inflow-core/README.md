# inflow-core

Shared configuration, authentication, transport interfaces, and errors for InFlow
MPP and x402 clients.

## Configuration

```rust,no_run
use inflow_core::{Authentication, ClientOptions, Environment};

fn main() -> Result<(), std::env::VarError> {
    let _options = ClientOptions {
        environment: Environment::Sandbox,
        authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
        ..ClientOptions::default()
    };
    Ok(())
}
```

Production is the default environment. The default request timeout is 30 seconds.
Authentication is one of anonymous, API key, or an asynchronous
`AccessTokenProvider`. The provider is called before every HTTP attempt, including
an allowed retry, so it can return a refreshed OAuth access token. It owns token
refresh; the SDK does not acquire or persist OAuth credentials.

API keys are sent as `X-API-KEY`; access tokens use `Authorization: Bearer`.
The SDK does not select or restrict account roles locally. If an endpoint requires
a Seller account, its server error code and explanation are preserved in `Error`.

## Runtime and transport

Async operations run on the application's Tokio runtime. The SDK does not start a
runtime or a background work queue. The default transport uses Reqwest, disables
redirects and its automatic retries, and bounds response bodies to 8 MiB.
The shared client also limits serialized request bodies to 8 MiB.

Set `ClientOptions::transport` to an `Arc<dyn Transport>` for a custom transport.
This is a trusted interface: it receives authentication headers and must not log
them or forward them through redirects. Dropping its returned future must stop
the attempt, including response-body reading. Test transports can route requests
to a local server without changing production or sandbox configuration.

Retries are selected by the protocol operation, not inferred from GET versus POST.
An operation can allow up to three retries for network failures, timeouts, and
HTTP 429, 502, 503, or 504. Backoff starts at 200 milliseconds and doubles, with
jitter. Authentication-provider failures are not retried. Each HTTP attempt has
its own timeout; polling has a separate deadline for the whole operation.

## Errors and cancellation

`Error` exposes the server code, message, HTTP status, endpoint, request identifier,
body, and response headers. Structured server errors retain their explanation;
an empty error response uses `UNEXPECTED_ERROR` and `request failed`.
Transport failures use `NETWORK_ERROR` or `TIMEOUT`.
Diagnostics remove sensitive headers and redact credential fields and authentication
values used during the request. Application-provided token errors remain the
provider's responsibility; do not include credentials in those messages.

Cancellation stops local polling and in-flight requests. Approval cleanup targets
only a known pending approval identifier. Explicit cancellation waits for one
attempt, bounded to five seconds independently of the cancelled operation.
Repeated callers share that attempt. Dropped-operation cleanup runs best-effort
on the existing runtime; exiting the process or shutting down that runtime can
prevent it from completing. Cancellation is not a payment reversal.

## Supported API boundary

Use the documented configuration, errors, and transport interfaces with the
protocol crates. `inflow_core::internal` contains shared implementation code used
by those crates and is hidden from generated documentation. It is technically
importable because Rust requires cross-crate symbols to be public, but it is not
a supported general-purpose endpoint client API.

See the [workspace documentation](https://github.com/inflowpayai/inflow-rust)
for the crate layout, environments, and repository verification commands.
