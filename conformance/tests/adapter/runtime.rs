use crate::{bad, read, string, transport};
use inflow_core::{
    AccessTokenProvider, Authentication, ClientOptions, Environment, Error, Transport,
    TransportError, TransportRequest, TransportResponse,
};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Tokens {
    values: Vec<String>,
    calls: AtomicUsize,
}
impl AccessTokenProvider for Tokens {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(async {
            self.values
                .get(self.calls.fetch_add(1, Ordering::SeqCst))
                .cloned()
                .ok_or_else(|| bad("unexpected token provider call"))
        })
    }
}
struct RuntimeTransport {
    inner: Option<Arc<dyn Transport>>,
    destinations: Arc<Mutex<Vec<String>>>,
}
impl Transport for RuntimeTransport {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            // Constructor capability setup is separate from the configuration error being measured.
            if request.method == http::Method::GET && request.url.ends_with("/v1/x402/supported") {
                return Ok(TransportResponse {
                    status: 200,
                    headers: Default::default(),
                    body: br#"{"kinds":[],"extensions":[],"signers":{}}"#.to_vec(),
                });
            }
            if let Some(inner) = &self.inner {
                return inner.send(request).await;
            }
            self.destinations
                .lock()
                .expect("destination lock")
                .push(format!("{} {}", request.method, request.url));
            Ok(TransportResponse {
                status: 403,
                headers: Default::default(),
                body: Vec::new(),
            })
        })
    }
}

async fn call(product: &str, options: ClientOptions) -> Result<(), Error> {
    let token = CancellationToken::new();
    match product {
        "mpp-buyer" => {
            let buyer = inflow_mpp_buyer::Buyer::new(options)?;
            let challenge = read(
                json!({"id":"test","realm":"seller.example","method":"inflow","intent":"charge","request":inflow_mpp::encode(&json!({"amount":"1","currency":"USD"}))?}),
            )?;
            let _payment = buyer
                .prepare(&challenge, Default::default(), &token)
                .await?;
        }
        "mpp-seller" => {
            inflow_mpp_seller::Seller::new(
                options,
                inflow_mpp_seller::SellerOptions {
                    realm: "seller.example".into(),
                    secret_key: "test-only-runtime-secret-at-least-32-bytes".into(),
                },
                &token,
            )
            .await?;
        }
        "x402-buyer" => {
            inflow_x402_buyer::Buyer::new(
                inflow_x402_buyer::BuyerOptions {
                    client: options,
                    ..Default::default()
                },
                &token,
            )
            .await?;
        }
        "x402-seller" => {
            inflow_x402_seller::Seller::new(options, &token).await?;
        }
        _ => return Err(bad("unknown runtime product")),
    }
    Err(bad("expected runtime failure"))
}

pub async fn execute(op: &str, input: &Value) -> Result<Value, Error> {
    let capture = match op {
        "runtime.environment" => true,
        "runtime.request" => false,
        _ => return Err(bad("unknown runtime operation")),
    };
    let destinations = Arc::new(Mutex::new(Vec::new()));
    let tokens = Arc::new(Tokens {
        values: input
            .get("tokens")
            .cloned()
            .map(read)
            .transpose()?
            .unwrap_or_default(),
        calls: AtomicUsize::new(0),
    });
    let mut options = if capture {
        ClientOptions::default()
    } else {
        transport::options(input)?
    };
    options.environment = match input["environment"].as_str() {
        None | Some("production") => Environment::Production,
        Some("sandbox") => Environment::Sandbox,
        _ => return Err(bad("unknown environment")),
    };
    options.authentication = if input.get("tokens").is_some() {
        Authentication::Bearer(tokens.clone())
    } else if let Some(key) = input["api_key"].as_str() {
        Authentication::ApiKey(key.into())
    } else {
        Authentication::Anonymous
    };
    options.transport = Some(Arc::new(RuntimeTransport {
        inner: options.transport.take(),
        destinations: destinations.clone(),
    }));
    let error = call(string(input, "product")?, options)
        .await
        .expect_err("runtime request must fail");
    if capture {
        if error.http_status != 403 {
            return Err(error);
        }
        return Ok(json!({"destinations":*destinations.lock().expect("destination lock")}));
    }
    if error.http_status == 0 {
        return Err(error);
    }
    let sensitive: Vec<_> = error
        .headers
        .keys()
        .map(|v| v.as_str())
        .filter(|v| matches!(*v, "authorization" | "cookie" | "set-cookie" | "x-api-key"))
        .collect();
    Ok(
        json!({"code":error.code,"message":error.message,"http_status":error.http_status,"endpoint":error.endpoint,"request_id":error.request_id.unwrap_or_default(),"token_calls":tokens.calls.load(Ordering::SeqCst),"sensitive_headers":sensitive}),
    )
}
