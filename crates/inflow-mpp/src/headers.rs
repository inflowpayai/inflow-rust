use crate::codec::{invalid, validate_challenge};
use crate::{Error, PaymentChallenge};
use serde_json::{Map, Value};

/// Parse repeated or comma-combined WWW-Authenticate values, retaining Payment challenges in order.
pub fn parse_challenges(headers: &[&str]) -> Result<Vec<PaymentChallenge>, Error> {
    let mut result = Vec::new();
    for header in headers {
        let mut current = None;
        for part in parts(header)? {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let first = part.split([' ', '\t', '=']).next().unwrap_or_default();
            let tail = part[first.len()..].trim_start();
            let parameter = tail.starts_with('=');
            if !parameter {
                finish(current.take(), &mut result)?;
                if first.eq_ignore_ascii_case("Payment") {
                    current = Some(Map::new());
                }
            }
            if let Some(fields) = &mut current {
                let pair = if parameter { part } else { tail };
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| invalid("header parameter"))?;
                let key = key.trim().to_ascii_lowercase();
                if key.is_empty() || !key.bytes().all(token) {
                    return Err(invalid("header parameter name"));
                }
                let value = unquote(value.trim())?;
                if fields.insert(key, Value::String(value)).is_some() {
                    return Err(invalid("duplicate header parameter"));
                }
            }
        }
        finish(current, &mut result)?;
    }
    Ok(result)
}

fn finish(
    fields: Option<Map<String, Value>>,
    result: &mut Vec<PaymentChallenge>,
) -> Result<(), Error> {
    if let Some(fields) = fields {
        // Deserialize preserves wire spelling; IntentName::new lowercases challenge-bound values.
        let challenge =
            serde_json::from_value(Value::Object(fields)).map_err(|_| invalid("challenge"))?;
        validate_challenge(&challenge)?;
        result.push(challenge);
    }
    Ok(())
}

fn parts(header: &str) -> Result<Vec<&str>, Error> {
    let (mut quoted, mut escaped, mut start) = (false, false, 0);
    let mut result = Vec::new();
    for (index, ch) in header.char_indices() {
        safe(ch)?;
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if !quoted && ch == ',' {
            result.push(&header[start..index]);
            start = index + 1;
        }
    }
    if quoted {
        return Err(invalid("unterminated quoted header"));
    }
    result.push(&header[start..]);
    Ok(result)
}

fn unquote(value: &str) -> Result<String, Error> {
    if !value.starts_with('"') {
        return if !value.is_empty() && value.bytes().all(token) {
            Ok(value.to_owned())
        } else {
            Err(invalid("header value"))
        };
    }
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| invalid("quoted header value"))?;
    let mut result = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // HTTP quoted-pair is not a JSON Unicode escape. See mpp-rs issue556.
            '\\' => result.push(chars.next().expect("header scan validated the quoted pair")),
            '"' => return Err(invalid("unescaped quote")),
            ch => result.push(ch),
        }
    }
    Ok(result)
}

fn token(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&ch)
}

fn safe(ch: char) -> Result<(), Error> {
    if (ch < ' ' && ch != '\t') || ch == '\u{7f}' {
        return Err(invalid("header control character"));
    }
    Ok(())
}

pub fn render_challenge(challenge: &PaymentChallenge) -> Result<String, Error> {
    validate_challenge(challenge)?;
    let mut parts = Vec::new();
    for (key, value) in [
        ("id", Some(challenge.id.as_str())),
        ("realm", Some(challenge.realm.as_str())),
        ("method", Some(challenge.method.as_str())),
        ("intent", Some(challenge.intent.as_str())),
        ("request", Some(challenge.request.raw())),
        ("expires", challenge.expires.as_deref()),
        ("description", challenge.description.as_deref()),
        ("digest", challenge.digest.as_deref()),
        ("opaque", challenge.opaque.as_ref().map(|v| v.raw())),
        ("header", challenge.header.as_deref()),
    ] {
        if let Some(value) = value {
            for ch in value.chars() {
                safe(ch)?;
            }
            let value = value.replace('\\', "\\\\").replace('"', "\\\"");
            parts.push(format!("{key}=\"{value}\""));
        }
    }
    Ok(format!("Payment {}", parts.join(", ")))
}
