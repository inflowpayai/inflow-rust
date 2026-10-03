use crate::{
    Buyer, CancellationToken, EncodedPayment, Error, OriginalJson, PaymentRequired, SignOptions,
    WaitOptions, buyer::cancelled, invalid,
};
use inflow_x402::{decode, encode, generate_payment_id, identifier_entry};
use reqwest::{Request, Response, StatusCode, header::HeaderValue};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};
use x402_types::{
    proto,
    scheme::client::{FirstMatch, PaymentSelector, X402SchemeClient},
};

/// Enriches external-wallet payments after signing; never receives hosted-wallet payments.
pub trait PaymentExtension: Send + Sync {
    fn enrich<'a>(
        &'a self,
        payload: Value,
        required: &'a PaymentRequired<OriginalJson>,
        cancellation: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Error>> + Send + 'a>>;
}

/// Owns a redirect-refusing merchant client, separate from the authenticated platform client.
pub struct HttpBuyer {
    http: reqwest::Client,
    buyer: Option<Buyer>,
    schemes: Vec<Arc<dyn X402SchemeClient>>,
    selector: Arc<dyn PaymentSelector>,
    extensions: Vec<Arc<dyn PaymentExtension>>,
}

impl HttpBuyer {
    /// None permits external-wallet-only use without platform authentication or capability calls.
    pub fn new(buyer: Option<Buyer>) -> Result<Self, Error> {
        let http = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(client) => client,
            Err(_) => return Err(invalid("could not construct merchant HTTP client")),
        };
        Ok(Self {
            http,
            buyer,
            schemes: Vec::new(),
            selector: Arc::new(FirstMatch),
            extensions: Vec::new(),
        })
    }

    pub fn register(mut self, scheme: impl X402SchemeClient + 'static) -> Self {
        self.schemes.push(Arc::new(scheme));
        self
    }

    pub fn with_extension(mut self, extension: impl PaymentExtension + 'static) -> Self {
        self.extensions.push(Arc::new(extension));
        self
    }

    /// Applies to external-wallet candidates. InFlow uses its asynchronous balance-aware selection first.
    pub fn with_selector(mut self, selector: impl PaymentSelector + 'static) -> Self {
        self.selector = Arc::new(selector);
        self
    }

    pub async fn payment(
        &self,
        required: &PaymentRequired<OriginalJson>,
        options: SignOptions,
        wait: WaitOptions,
        cancellation: &CancellationToken,
    ) -> Result<EncodedPayment, Error> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            result = self.sign(required, options, wait, cancellation) => result,
        }
    }

    async fn sign(
        &self,
        required: &PaymentRequired<OriginalJson>,
        options: SignOptions,
        wait: WaitOptions,
        cancellation: &CancellationToken,
    ) -> Result<EncodedPayment, Error> {
        if let Some(buyer) = &self.buyer
            && let Some(requirement) = buyer.select(required, cancellation).await?
        {
            return buyer
                .sign(&requirement, required, options, wait, cancellation)
                .await;
        }
        let required_wire = proto::PaymentRequired::V2(required.clone());
        let candidates: Vec<_> = self
            .schemes
            .iter()
            .flat_map(|scheme| scheme.accept(&required_wire))
            .collect();
        let selected = self
            .selector
            .select(&candidates)
            .ok_or_else(|| Error::new("X402_NO_MATCH", "no supported payment option"))?;
        let encoded = selected
            .sign()
            .await
            .map_err(|_| Error::new("X402_SIGNING_FAILED", "external wallet signing failed"))?;
        let mut payload = decode(&encoded)?;
        // The upstream exact signer copies this declaration without producing an allowance permit.
        // An unsigned advertisement must not be sent as sponsorship; custom signed permits remain intact.
        if payload["accepted"]["scheme"] == "exact"
            && let Some(extensions) = payload.get_mut("extensions").and_then(Value::as_object_mut)
            && extensions.get("eip2612GasSponsoring").is_some_and(|entry| {
                entry["info"]["signature"]
                    .as_str()
                    .is_none_or(str::is_empty)
            })
        {
            extensions.remove("eip2612GasSponsoring");
        }
        for extension in &self.extensions {
            payload = extension.enrich(payload, required, cancellation).await?;
        }
        let declarations = required.extensions.as_ref();
        if let Some(declaration) = declarations.get("payment-identifier") {
            let id = generate_payment_id("pay_")?;
            match identifier_entry(declaration, &id) {
                Some(entry) => {
                    let object = payload
                        .as_object_mut()
                        .ok_or_else(|| invalid("external signer returned a non-object payload"))?;
                    let extensions = object
                        .entry("extensions")
                        .or_insert_with(|| serde_json::json!({}));
                    extensions
                        .as_object_mut()
                        .ok_or_else(|| invalid("external signer returned non-object extensions"))?
                        .insert("payment-identifier".into(), entry);
                }
                None if declaration["info"]["required"] == true => {
                    return Err(invalid(
                        "required payment-identifier declaration is invalid",
                    ));
                }
                None => {}
            }
        }
        Ok(EncodedPayment {
            encoded_payload: encode(&payload),
            payment_payload: payload,
            transaction_id: None,
        })
    }

    /// Sends once, then at most one paid retry of the same request. Streaming bodies are rejected before sending.
    pub async fn execute(
        &self,
        request: Request,
        options: SignOptions,
        wait: WaitOptions,
        cancellation: &CancellationToken,
    ) -> Result<Response, Error> {
        // Upstream signs before discovering that retry cloning failed. Reject here before any approval or signature.
        let mut retry = request.try_clone().ok_or_else(|| {
            Error::new(
                "X402_REQUEST_NOT_REPLAYABLE",
                "automatic payment requires a replayable request body",
            )
        })?;
        if !matches!(request.url().scheme(), "https" | "http")
            || !request.url().username().is_empty()
            || request.url().password().is_some()
        {
            return Err(invalid(
                "merchant URL must be HTTP or HTTPS without embedded credentials",
            ));
        }
        let already_paid = request.headers().contains_key("payment-signature")
            || request.headers().contains_key("x-payment");
        let response = self.send(request, cancellation).await?;
        if response.status() != StatusCode::PAYMENT_REQUIRED || already_paid {
            return Ok(response);
        }
        let header = response
            .headers()
            .get("payment-required")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| invalid("402 response has no V2 Payment-Required header"))?;
        let required: PaymentRequired<OriginalJson> = serde_json::from_value(decode(header)?)
            .map_err(|_| invalid("invalid V2 payment requirements"))?;
        drop(response);
        let payment = self.payment(&required, options, wait, cancellation).await?;
        let mut header = HeaderValue::from_str(&payment.encoded_payload)
            .map_err(|_| invalid("invalid payment header"))?;
        header.set_sensitive(true);
        retry.headers_mut().insert("payment-signature", header);
        // Never follow a merchant redirect with payment or service-authentication credentials.
        self.send(retry, cancellation).await
    }

    async fn send(
        &self,
        request: Request,
        cancellation: &CancellationToken,
    ) -> Result<Response, Error> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            response = self.http.execute(request) => response.map_err(|_| Error::new("X402_RESOURCE_HTTP_ERROR", "merchant HTTP request failed")),
        }
    }
}
