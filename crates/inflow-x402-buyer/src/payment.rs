use crate::{
    Buyer, CancellationToken, Error, OriginalJson, PaymentRequired,
    buyer::{cancelled, read},
    invalid,
};
use inflow_core::internal::ApprovalCleanup;
use inflow_x402::valid_payment_id;
use serde_json::{Map, Value, json};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct SignOptions {
    pub payment_id: Option<String>,
    /// Additional creation fields. Cannot override accept, resource, version, or payment ID.
    pub transaction_fields: Map<String, Value>,
}

#[derive(Clone, Copy)]
pub struct WaitOptions {
    pub poll_interval: Duration,
    /// Budget starting when wait is called; creation has the platform HTTP timeout.
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

/// Contains a payment credential; deliberately does not implement Debug.
pub struct EncodedPayment {
    pub encoded_payload: String,
    pub payment_payload: Value,
    pub transaction_id: Option<String>,
}

/// Owns one approval. Dropping a pending handle schedules bounded best-effort cleanup.
pub struct Payment {
    buyer: Buyer,
    approval_id: String,
    transaction_id: String,
    approved: bool,
    cleanup: ApprovalCleanup,
    cancellation: CancellationToken,
}

impl Buyer {
    pub async fn prepare(
        &self,
        requirement: &OriginalJson,
        required: &PaymentRequired<OriginalJson>,
        options: SignOptions,
        cancellation: &CancellationToken,
    ) -> Result<Payment, Error> {
        if !self.supports(requirement, cancellation).await? {
            return Err(Error::new(
                "X402_ADAPTER_ROUTING_ERROR",
                "payment requirements are not supported by InFlow signing",
            ));
        }
        if options
            .payment_id
            .as_deref()
            .is_some_and(|id| !valid_payment_id(id))
        {
            return Err(Error::new(
                "X402_PAYMENT_ID_FORMAT",
                "invalid payment identifier",
            ));
        }
        let mut body = options.transaction_fields;
        body.remove("remotePaymentId");
        body.insert("accept".into(), read(requirement)?);
        body.insert("resource".into(), json!(required.resource));
        body.insert("x402Version".into(), json!(2));
        if body["accept"]["scheme"] == "instrument"
            && let Some(id) = &self.instrument_id
        {
            body.insert("instrumentId".into(), json!(id));
        }
        if let Some(id) = options.payment_id {
            body.insert("remotePaymentId".into(), json!(id));
        }
        let cancellation = cancellation.child_token();
        let created = self
            .client
            .create(Value::Object(body), &cancellation)
            .await?;
        let approval_id = field(&created, "approvalId")?.to_owned();
        // Arm cleanup as soon as an approval is known, even if the remaining response is malformed.
        let cleanup = self.client.approval_cleanup(&approval_id)?;
        let transaction_id = field(&created, "transactionId")?.to_owned();
        Ok(Payment {
            buyer: self.clone(),
            approval_id,
            transaction_id,
            approved: created["approvalStatus"] == "APPROVED",
            cleanup,
            cancellation,
        })
    }

    pub async fn sign(
        &self,
        requirement: &OriginalJson,
        required: &PaymentRequired<OriginalJson>,
        options: SignOptions,
        wait: WaitOptions,
        cancellation: &CancellationToken,
    ) -> Result<EncodedPayment, Error> {
        self.prepare(requirement, required, options, cancellation)
            .await?
            .wait(wait)
            .await
    }
}

impl Payment {
    pub fn approval_id(&self) -> &str {
        &self.approval_id
    }
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
    pub async fn status(&self) -> Result<Value, Error> {
        self.buyer
            .payload(&self.transaction_id, &self.cancellation)
            .await
    }
    pub async fn cancel(&self) -> Result<(), Error> {
        self.cancellation.cancel();
        self.cleanup.cancel().await
    }
    /// Consumes this handle; concurrent callers cannot create duplicate polling loops.
    ///
    /// ```compile_fail,E0382
    /// async fn wait_twice(payment: inflow_x402_buyer::Payment) {
    ///     let options = inflow_x402_buyer::WaitOptions::default();
    ///     let _ = payment.wait(options).await;
    ///     let _ = payment.wait(options).await;
    /// }
    /// ```
    pub async fn wait(mut self, options: WaitOptions) -> Result<EncodedPayment, Error> {
        let cancellation = self.cancellation.clone();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            result = tokio::time::timeout(options.timeout, self.resolve(options.poll_interval)) => result.unwrap_or_else(|_| Err(Error::new("X402_APPROVAL_TIMEOUT", "x402 approval timed out"))),
        };
        if result.is_err() {
            let _ = self.cleanup.cancel().await;
        }
        self.cleanup.disarm();
        result
    }
    async fn resolve(&mut self, interval: Duration) -> Result<EncodedPayment, Error> {
        loop {
            let response = self
                .buyer
                .payload(&self.transaction_id, &self.cancellation)
                .await;
            match response {
                Ok(value) => {
                    if let Some(encoded) =
                        value["encodedPayload"].as_str().filter(|s| !s.is_empty())
                        && value["paymentPayload"].is_object()
                    {
                        return Ok(EncodedPayment {
                            encoded_payload: encoded.into(),
                            payment_payload: value["paymentPayload"].clone(),
                            transaction_id: Some(self.transaction_id.clone()),
                        });
                    }
                    if matches!(
                        value["status"].as_str(),
                        Some("DECLINED" | "EXPIRED" | "GENERAL_ERROR" | "INSUFFICIENT_FUNDS")
                    ) {
                        let mut error =
                            Error::new("X402_APPROVAL_FAILED", "x402 payment approval failed");
                        error.body = Box::new(
                            json!({"approvalId":self.approval_id,"transactionId":self.transaction_id,"status":value["status"]}),
                        );
                        return Err(error);
                    }
                }
                Err(error)
                    if error.http_status == 429
                        || error.http_status >= 500
                        || matches!(error.code.as_str(), "NETWORK_ERROR" | "TIMEOUT") => {}
                Err(error) => return Err(error),
            }
            if self.approved {
                self.approved = false;
            } else {
                tokio::time::sleep(interval).await;
            }
        }
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("creation response is missing an identifier"))
}
