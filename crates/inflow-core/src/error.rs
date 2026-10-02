use http::HeaderMap;
use serde_json::Value;
use std::fmt;

#[derive(Clone, Debug)]
pub struct Error {
    pub code: String,
    pub message: String,
    pub http_status: u16,
    pub endpoint: String,
    pub request_id: Option<String>,
    pub body: Box<Value>,
    pub headers: Box<HeaderMap>,
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            http_status: 0,
            endpoint: String::new(),
            request_id: None,
            body: Box::new(Value::Null),
            headers: Box::default(),
        }
    }

    pub(crate) fn invalid(reason: impl fmt::Display) -> Self {
        Self::new(
            "INVALID_CONFIGURATION",
            format!("invalid client configuration: {reason}"),
        )
    }

    pub(crate) fn response(
        path: &str,
        status: u16,
        headers: HeaderMap,
        body: Value,
        secrets: &[String],
    ) -> Self {
        let body = redact(body, secrets);
        let first = body
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|a| a.first());
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let code = text(first.and_then(|v| v.get("code")))
            .or_else(|| text(body.get("code")))
            .unwrap_or_else(|| "UNEXPECTED_ERROR".into());
        let message = text(body.get("detail"))
            .or_else(|| text(first.and_then(|v| v.get("message"))))
            .or_else(|| text(body.get("message")))
            .unwrap_or_else(|| "request failed".into());
        let mut safe_headers = HeaderMap::new();
        for (name, value) in &headers {
            if !sensitive(name.as_str())
                && let Ok(value) = value.to_str()
                && let Ok(value) = scrub(value, secrets).parse()
            {
                safe_headers.append(name.clone(), value);
            }
        }
        Self {
            code,
            message,
            http_status: status,
            endpoint: scrub(path, secrets),
            request_id: safe_headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            body: Box::new(body),
            headers: Box::new(safe_headers),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

fn sensitive(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "xapikey"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "privatekey"
            | "secretkey"
            | "password"
            | "credential"
            | "signature"
            | "paymentsignature"
            | "xpayment"
    )
}

pub(crate) fn scrub(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<_> = secrets.iter().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.into_iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret, "[REDACTED]")
    })
}

fn redact(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(s) => Value::String(scrub(&s, secrets)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| redact(v, secrets)).collect())
        }
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(k, v)| {
                    let v = if sensitive(&k) {
                        Value::String("[REDACTED]".into())
                    } else {
                        redact(v, secrets)
                    };
                    (scrub(&k, secrets), v)
                })
                .collect(),
        ),
        v => v,
    }
}
