use crate::{
    Buyer, CancellationToken, CardPaymentOptions, Error, MethodLimits, PaymentChallenge,
    PaymentOptions, WaitOptions,
};
use mpp::{
    MppError, PaymentCredential,
    client::{ChargeClient, HttpError, PaymentProvider},
};
use reqwest::{RequestBuilder, Response};
use serde_json::{Value, json};
use std::sync::Arc;

/// One explicitly enabled charge method and its caller-selected options.
#[derive(Clone)]
pub enum HttpPaymentMethod {
    Inflow { instrument_id: Option<String> },
    Tempo,
    Card(CardPaymentOptions),
}

type ErrorHandler = dyn Fn(&PaymentChallenge, &Error) + Send + Sync;

/// Automatic charge payments using upstream's redirect-disabled HTTP client.
#[derive(Clone)]
pub struct HttpPayment {
    buyer: Buyer,
    method: HttpPaymentMethod,
    wait: WaitOptions,
    cancellation: CancellationToken,
    on_error: Option<Arc<ErrorHandler>>,
    limits: MethodLimits,
}

impl HttpPayment {
    pub fn new(buyer: Buyer, method: HttpPaymentMethod) -> Self {
        Self {
            buyer,
            method,
            wait: WaitOptions::default(),
            cancellation: CancellationToken::new(),
            on_error: None,
            limits: MethodLimits::default(),
        }
    }

    pub fn new_with_limits(
        buyer: Buyer,
        method: HttpPaymentMethod,
        limits: MethodLimits,
    ) -> Result<Self, Error> {
        limits.validate(method.name())?;
        Ok(Self {
            limits,
            ..Self::new(buyer, method)
        })
    }

    /// Replaces or clears the request's instrument without changing its method or limits.
    pub fn with_instrument_id(mut self, instrument_id: Option<String>) -> Result<Self, Error> {
        match &mut self.method {
            HttpPaymentMethod::Inflow {
                instrument_id: current,
            } => *current = instrument_id,
            HttpPaymentMethod::Card(options) => options.instrument_id = instrument_id,
            HttpPaymentMethod::Tempo => {
                return Err(Error::new(
                    "MPP_INVALID_OPTIONS",
                    "Tempo does not accept an instrument selector",
                ));
            }
        }
        Ok(self)
    }

    pub fn with_wait_options(mut self, options: WaitOptions) -> Self {
        self.wait = options;
        self
    }

    /// Cancels credential creation and approval waiting, not resource-response delivery.
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Observes the original InFlow error before conversion to the upstream error type.
    /// The synchronous callback must not block or panic; it does not authorize a retry.
    pub fn with_error_handler(
        mut self,
        handler: impl Fn(&PaymentChallenge, &Error) + Send + Sync + 'static,
    ) -> Self {
        self.on_error = Some(Arc::new(handler));
        self
    }

    /// Requires a request-level Accept-Payment header to authorize automatic payment.
    pub async fn send(
        &self,
        client: &ChargeClient,
        request: RequestBuilder,
    ) -> Result<Response, HttpError> {
        client.send_with_payment(request, &Provider(self)).await
    }

    async fn credential(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential, Error> {
        self.limits.check(challenge)?;
        if self.cancellation.is_cancelled() {
            return Err(Error::new("MPP_PAYMENT_CANCELLED", "payment cancelled"));
        }
        let payment = match &self.method {
            HttpPaymentMethod::Card(options) => {
                self.buyer
                    .prepare_card(challenge, options.clone(), &self.cancellation)
                    .await?
            }
            method => {
                let instrument_id = match method {
                    HttpPaymentMethod::Inflow { instrument_id } => instrument_id.clone(),
                    _ => None,
                };
                self.buyer
                    .prepare(
                        challenge,
                        PaymentOptions {
                            instrument_id,
                            subscription_id: None,
                        },
                        &self.cancellation,
                    )
                    .await?
            }
        };
        let credential = payment.wait(self.wait).await?;
        if json!(credential.challenge) != json!(challenge) {
            return Err(Error::new(
                "MPP_MALFORMED_CREDENTIAL",
                "payment credential does not match the requested challenge",
            ));
        }
        Ok(PaymentCredential {
            challenge: credential.challenge.to_echo(),
            payload: Value::Object(credential.payload),
            source: credential.source,
        })
    }

    fn report(&self, challenge: &PaymentChallenge, error: Error) -> MppError {
        if let Some(handler) = &self.on_error {
            handler(challenge, &error);
        }
        let mut message = format!("{}: {}", error.code, error.message);
        if let Some(id) = &error.request_id {
            message.push_str(&format!(" (request {id})"));
        }
        if let Some(id) = error.body.get("transactionId").and_then(Value::as_str) {
            message.push_str(&format!(" (transaction {id})"));
        }
        MppError::Http(message)
    }
}

impl HttpPaymentMethod {
    fn name(&self) -> &str {
        match self {
            Self::Inflow { .. } => "inflow",
            Self::Tempo => "tempo",
            Self::Card(_) => "card",
        }
    }
}

// Keep the native provider private: MCP retries alternative offers after uncertain failures.
// https://github.com/tempoxyz/mpp-rs/issues/727 must be fixed before exposing it to MCP.
// Require a real upstream regression test proving no second payment after a lost response,
// without inventing a blockchain hash or treating a one-time charge as a session.
#[derive(Clone)]
struct Provider<'a>(&'a HttpPayment);

impl PaymentProvider for Provider<'_> {
    fn supports(&self, method: &str, intent: &str) -> bool {
        intent == "charge" && method == self.0.method.name()
    }

    fn select_challenge<'a>(
        &self,
        challenges: &[&'a PaymentChallenge],
    ) -> Option<&'a PaymentChallenge> {
        challenges
            .iter()
            .copied()
            .find(|challenge| self.0.limits.check(challenge).is_ok())
    }

    async fn pay(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
        self.0
            .credential(challenge)
            .await
            .map_err(|error| self.0.report(challenge, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientOptions;
    use inflow_mpp::Base64UrlJson;

    #[tokio::test]
    async fn credential_creation_rechecks_limits_without_relying_on_selection() {
        let payment = HttpPayment::new_with_limits(
            Buyer::new(ClientOptions::default()).unwrap(),
            HttpPaymentMethod::Inflow {
                instrument_id: None,
            },
            MethodLimits {
                max_amount: Some("0".into()),
                currencies: Some(vec!["USDC".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let challenge = PaymentChallenge::new(
            "id",
            "shop",
            "inflow",
            "charge",
            Base64UrlJson::from_value(&json!({"amount":"1","currency":"USDC"})).unwrap(),
        );
        assert_eq!(
            payment.credential(&challenge).await.unwrap_err().code,
            "MPP_PAYMENT_LIMIT"
        );
        assert!(
            HttpPayment::new_with_limits(
                Buyer::new(ClientOptions::default()).unwrap(),
                HttpPaymentMethod::Tempo,
                MethodLimits {
                    max_amount: Some("1".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
