use crate::{CancellationToken, ClientOptions, Credential, Error, PaymentChallenge};
use inflow_core::internal::ApprovalCleanup;
use inflow_mpp::{
    decode, decode_credential, internal::MppClient, render_challenge, validate_request,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Default)]
pub struct PaymentOptions {
    pub instrument_id: Option<String>,
    /// An existing subscription, not an approval identifier.
    pub subscription_id: Option<String>,
}

#[derive(Clone, Copy)]
pub struct WaitOptions {
    /// Used only when the platform supplies no retryAfterSeconds. Zero permits immediate polling.
    pub poll_interval: Duration,
    /// Pending budget measured from the creation response, including polling requests and delays.
    pub timeout: Duration,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            timeout: Duration::from_secs(900),
        }
    }
}

#[derive(Clone)]
pub struct Buyer(MppClient);

impl Buyer {
    pub fn new(options: ClientOptions) -> Result<Self, Error> {
        Ok(Self(MppClient::new(options)?))
    }

    /// Creates once, or authorizes an existing subscription. Does not send a resource request.
    pub async fn prepare(
        &self,
        challenge: &PaymentChallenge,
        options: PaymentOptions,
        cancellation: &CancellationToken,
    ) -> Result<Payment, Error> {
        render_challenge(challenge)?;
        validate_request(
            challenge.method.as_str(),
            challenge.intent.as_str(),
            &decode(challenge.request.raw())?,
        )?;
        if challenge.is_expired() {
            return Err(expired());
        }
        for id in [&options.instrument_id, &options.subscription_id]
            .into_iter()
            .flatten()
        {
            if !guid(id) {
                return Err(malformed("invalid payment option identifier"));
            }
        }
        if options.instrument_id.is_some()
            && (challenge.method.as_str() != "inflow" || challenge.intent.as_str() != "charge")
            || options.subscription_id.is_some()
                && (challenge.method.as_str() != "inflow"
                    || challenge.intent.as_str() != "subscription")
        {
            return Err(malformed(
                "payment options do not apply to this method and intent",
            ));
        }
        let cancellation = cancellation.child_token();
        let wire = json!(challenge);
        let response = if let Some(id) = options.subscription_id {
            let response = self
                .0
                .authorize(&id, wire, &cancellation)
                .await
                .map_err(payment_error)?;
            if response.get("problem").is_some_and(|v| !v.is_null()) {
                return Err(failed(&response));
            }
            json!({"state":"ready", "credential":response.get("credential")})
        } else {
            let mut body_options = json!({});
            if let Some(id) = options.instrument_id {
                body_options["instrumentId"] = json!(id);
            }
            self.0
                .create(
                    json!({"challenge":wire,"options":body_options}),
                    &cancellation,
                )
                .await
                .map_err(payment_error)?
        };
        let received = Instant::now();
        let cleanup = if response["state"] == "pending" {
            optional_id(&response, "approvalId")?
                .map(|id| self.0.approval_cleanup(id))
                .transpose()?
        } else {
            None
        };
        Ok(Payment {
            client: self.0.clone(),
            response,
            received,
            cleanup,
            cancellation,
        })
    }

    /// Cancels one known approval; does not cancel a subscription or reverse a payment.
    pub async fn cancel_approval(&self, approval_id: &str) -> Result<(), Error> {
        self.0.approval_cleanup(approval_id)?.cancel().await
    }
}

/// A prepared operation. Dropping a pending handle schedules bounded approval cleanup.
/// It contains payment credentials and deliberately does not implement Debug or Clone.
pub struct Payment {
    client: MppClient,
    response: Value,
    received: Instant,
    cleanup: Option<ApprovalCleanup>,
    cancellation: CancellationToken,
}

impl Payment {
    pub fn approval_id(&self) -> Option<&str> {
        self.response["approvalId"].as_str()
    }
    pub fn transaction_id(&self) -> Option<&str> {
        self.response["transactionId"].as_str()
    }

    /// Cancelling this token affects this payment only, not other calls on the Buyer.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Stops this operation and waits for one coalesced approval-cancellation attempt.
    pub async fn cancel(&self) -> Result<(), Error> {
        self.cancellation.cancel();
        if let Some(cleanup) = &self.cleanup {
            cleanup.cancel().await?;
        }
        Ok(())
    }

    pub async fn wait(mut self, options: WaitOptions) -> Result<Credential, Error> {
        let cancellation = self.cancellation.clone();
        let mut result = if self.response["state"] == "pending" {
            match self.received.checked_add(options.timeout) {
                Some(deadline) => {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Err(cancelled()),
                        result = tokio::time::timeout_at(deadline, self.resolve(options.poll_interval, Some(deadline))) => result.unwrap_or_else(|_| Err(timed_out())),
                    }
                }
                None => Err(malformed("pending timeout is too large")),
            }
        } else if cancellation.is_cancelled() {
            Err(cancelled())
        } else {
            self.resolve(options.poll_interval, None).await
        };
        if let Err(error) = &mut result
            && matches!(
                error.code.as_str(),
                "MPP_PAYMENT_TIMEOUT" | "MPP_PAYMENT_EXPIRED"
            )
        {
            *error.body = json!({"transactionId": self.transaction_id()});
        }
        if let Some(cleanup) = &mut self.cleanup {
            if result.is_err() {
                // Cleanup has its own five-second budget and must not replace the original failure.
                let _ = cleanup.cancel().await;
            }
            cleanup.disarm();
        }
        result
    }

    async fn resolve(
        &mut self,
        poll_interval: Duration,
        deadline: Option<Instant>,
    ) -> Result<Credential, Error> {
        loop {
            check_deadline(deadline)?;
            match self.response["state"].as_str() {
                Some("ready") => {
                    let encoded = self.response["credential"]
                        .as_str()
                        .ok_or_else(|| malformed("ready response has no credential"))?;
                    return decode_credential(encoded)
                        .map_err(|_| malformed("ready response has an invalid credential"));
                }
                Some("failed") => return Err(failed(&self.response)),
                Some("expired") => return Err(expired()),
                Some("pending") => {
                    if self.cleanup.is_none() {
                        self.cleanup = optional_id(&self.response, "approvalId")?
                            .map(|id| self.client.approval_cleanup(id))
                            .transpose()?;
                    }
                }
                _ => return Err(malformed("unknown transaction state")),
            }
            let id = optional_id(&self.response, "transactionId")?
                .ok_or_else(|| malformed("pending response has no transactionId"))?
                .to_owned();
            let delay = match self.response.get("retryAfterSeconds") {
                None => poll_interval,
                Some(value) => Duration::try_from_secs_f64(
                    value
                        .as_f64()
                        .filter(|v| *v >= 0.0)
                        .ok_or_else(|| malformed("retryAfterSeconds"))?,
                )
                .map_err(|_| malformed("retryAfterSeconds"))?,
            };
            let delay = deadline.map_or(delay, |deadline| {
                delay.min(deadline.saturating_duration_since(Instant::now()))
            });
            tokio::time::sleep(delay).await;
            check_deadline(deadline)?;
            self.response = self
                .client
                .poll_transaction(&id, &self.cancellation)
                .await
                .map_err(payment_error)?;
        }
    }
}

fn guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn optional_id<'a>(response: &'a Value, field: &str) -> Result<Option<&'a str>, Error> {
    response
        .get(field)
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| malformed("response identifier"))
        })
        .transpose()
}
fn malformed(message: &str) -> Error {
    Error::new("MPP_MALFORMED_CREDENTIAL", message)
}
fn expired() -> Error {
    Error::new("MPP_PAYMENT_EXPIRED", "MPP payment expired")
}
fn cancelled() -> Error {
    Error::new("MPP_PAYMENT_CANCELLED", "MPP payment cancelled by caller")
}
fn timed_out() -> Error {
    Error::new("MPP_PAYMENT_TIMEOUT", "MPP payment approval timed out")
}
fn check_deadline(deadline: Option<Instant>) -> Result<(), Error> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(timed_out())
    } else {
        Ok(())
    }
}
fn payment_error(error: Error) -> Error {
    if error.code == "CANCELLED" {
        cancelled()
    } else {
        error
    }
}
fn failed(response: &Value) -> Error {
    let problem = &response["problem"];
    let message = problem["detail"]
        .as_str()
        .or_else(|| problem["title"].as_str())
        .unwrap_or("MPP payment failed");
    let mut error = Error::new("MPP_PAYMENT_FAILED", message);
    error.body = Box::new(problem.clone());
    error
}
