use crate::{Error, Intent, Request, VerifiedFacts, invalid};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use sha2::{Digest, Sha256};

pub(crate) struct Prepared {
    pub facts: VerifiedFacts,
    pub base: String,
    pub signature: Vec<u8>,
}

pub(crate) fn base64(value: &str) -> Result<Vec<u8>, Error> {
    GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    )
    .decode(value)
    .map_err(|_| invalid())
}

#[derive(Clone)]
enum Item {
    String(String),
    Integer(i64),
    Other,
}

struct Cursor<'a>(&'a str);
impl Cursor<'_> {
    fn take(&mut self, prefix: &str) -> Result<(), Error> {
        self.0 = self.0.strip_prefix(prefix).ok_or_else(invalid)?;
        Ok(())
    }
    fn string(&mut self) -> Result<String, Error> {
        self.take("\"")?;
        let mut out = String::new();
        loop {
            let byte = *self.0.as_bytes().first().ok_or_else(invalid)?;
            self.0 = &self.0[1..];
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let next = *self.0.as_bytes().first().ok_or_else(invalid)?;
                    if !matches!(next, b'"' | b'\\') {
                        return Err(invalid());
                    }
                    self.0 = &self.0[1..];
                    out.push(char::from(next));
                }
                32..=126 => out.push(char::from(byte)),
                _ => return Err(invalid()),
            }
        }
    }
    fn item(&mut self) -> Result<Item, Error> {
        if self.0.starts_with('"') {
            return self.string().map(Item::String);
        }
        let n = self.0.find(';').unwrap_or(self.0.len());
        let value = &self.0[..n];
        self.0 = &self.0[n..];
        if value.starts_with(':') && value.ends_with(':') && value.len() >= 2 {
            base64(&value[1..value.len() - 1])?;
        } else if value == "?0" || value == "?1" {
        } else if value.starts_with('-') || value.starts_with(|c: char| c.is_ascii_digit()) {
            let digits = value.strip_prefix('-').unwrap_or(value);
            if let Some((whole, fraction)) = digits.split_once('.') {
                if whole.is_empty()
                    || whole.len() > 12
                    || fraction.is_empty()
                    || fraction.len() > 3
                    || !whole
                        .bytes()
                        .chain(fraction.bytes())
                        .all(|c| c.is_ascii_digit())
                {
                    return Err(invalid());
                }
            } else {
                if digits.is_empty()
                    || digits.len() > 15
                    || !digits.bytes().all(|c| c.is_ascii_digit())
                {
                    return Err(invalid());
                }
                // Fifteen validated decimal digits fit in i64, including the sign.
                let number = digits
                    .bytes()
                    .fold(0_i64, |n, c| n * 10 + i64::from(c - b'0'));
                return Ok(Item::Integer(if value.starts_with('-') {
                    -number
                } else {
                    number
                }));
            }
        } else if !value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '*')
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~:/-".contains(&c))
        {
            return Err(invalid());
        }
        Ok(Item::Other)
    }
}

fn header<'a>(request: &'a Request, name: &str) -> Result<&'a str, Error> {
    let mut values = request.headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(invalid)?
        .to_str()
        .map_err(|_| invalid())?;
    if values.next().is_some() {
        return Err(invalid());
    }
    Ok(value)
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn prepare(request: &Request, now: i64) -> Result<Prepared, Error> {
    let input = header(request, "signature-input")?;
    let mut cursor = Cursor(input.trim_start_matches(' ').trim_end_matches([' ', '\t']));
    cursor.take("sig2=(")?;
    cursor.0 = cursor.0.trim_start_matches(' ');
    let mut components = Vec::new();
    loop {
        let component = cursor.string()?;
        if components.contains(&component) {
            return Err(invalid());
        }
        components.push(component);
        if cursor.0.starts_with(')') {
            break;
        }
        cursor.take(" ")?;
        cursor.0 = cursor.0.trim_start_matches(' ');
        if cursor.0.starts_with(')') {
            break;
        }
    }
    cursor.take(")")?;
    let mut params: Vec<(String, Item)> = Vec::new();
    while !cursor.0.is_empty() {
        cursor.take(";")?;
        cursor.0 = cursor.0.trim_start_matches(' ');
        let end = cursor.0.find(['=', ';']).unwrap_or(cursor.0.len());
        let name = cursor.0[..end].to_owned();
        if !["created", "expires", "keyid", "alg", "nonce", "tag"].contains(&name.as_str()) {
            return Err(invalid());
        }
        cursor.0 = &cursor.0[end..];
        let item = if cursor.0.starts_with('=') {
            cursor.take("=")?;
            cursor.item()?
        } else {
            Item::Other
        };
        // Structured Fields retains the first position and last value, including type.
        if let Some((_, value)) = params.iter_mut().find(|(key, _)| key == &name) {
            *value = item;
        } else {
            params.push((name, item));
        }
    }
    let get = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v)
            .ok_or_else(invalid)
    };
    let integer = |name| match get(name)? {
        Item::Integer(n) => Ok(*n),
        _ => Err(invalid()),
    };
    let string = |name| match get(name)? {
        Item::String(s) if !s.is_empty() => Ok(s.clone()),
        _ => Err(invalid()),
    };
    let created = integer("created")?;
    let expires = integer("expires")?;
    let keyid = string("keyid")?;
    let nonce = string("nonce")?;
    if !["ed25519", "Ed25519"].contains(&string("alg")?.as_str()) {
        return Err(invalid());
    }
    let intent = match string("tag")?.as_str() {
        "agent-browser-auth" => Intent::Browse,
        "agent-payer-auth" => Intent::Pay,
        _ => return Err(invalid()),
    };
    let mut expected = vec!["@method", "@authority", "@path", "@query"];
    if request.body.is_some() {
        expected.extend(["content-digest", "content-type"]);
    }
    if components.len() != expected.len()
        || expected.iter().any(|c| !components.iter().any(|v| v == c))
    {
        return Err(invalid());
    }
    if expires <= created || expires - created > 480 {
        return Err(Error::new(
            "SIGNATURE_LIFETIME_INVALID",
            "The TAP signature lifetime is invalid.",
        ));
    }
    if now < created {
        return Err(Error::new(
            "SIGNATURE_NOT_YET_VALID",
            "The TAP signature is not yet valid.",
        ));
    }
    if now >= expires {
        return Err(Error::new(
            "SIGNATURE_EXPIRED",
            "The TAP signature has expired.",
        ));
    }
    let url = url::Url::parse(&request.url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    if let Some(body) = &request.body {
        header(request, "content-type")?;
        let digest = header(request, "content-digest").map_err(|_| {
            Error::new(
                "CONTENT_DIGEST_INVALID",
                "The TAP content digest is invalid.",
            )
        })?;
        if digest
            != format!(
                "sha-256=:{}:",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body))
            )
        {
            return Err(Error::new(
                "CONTENT_DIGEST_INVALID",
                "The TAP content digest is invalid.",
            ));
        }
    }
    let mut base = String::new();
    for component in &components {
        let value = match component.as_str() {
            "@method" => request.method.clone(),
            "@authority" => url[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
            "@path" => url.path().to_owned(),
            "@query" => format!("?{}", url.query().unwrap_or_default()),
            name => header(request, name)?.to_owned(),
        };
        base.push_str(&format!("{}: {}\n", quoted(component), value));
    }
    base.push_str(&format!(
        "\"@signature-params\": ({})",
        components
            .iter()
            .map(|v| quoted(v))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for (name, item) in params {
        let value = match item {
            Item::Integer(n) => n.to_string(),
            Item::String(s) => quoted(&s),
            Item::Other => return Err(invalid()),
        };
        base.push_str(&format!(";{name}={value}"));
    }
    let signature = header(request, "signature")?
        .trim_start_matches(' ')
        .trim_end_matches([' ', '\t']);
    let signature = signature
        .strip_prefix("sig2=:")
        .and_then(|s| s.strip_suffix(':'))
        .ok_or_else(invalid)?;
    Ok(Prepared {
        facts: VerifiedFacts {
            keyid,
            algorithm: "ed25519",
            intent,
            nonce,
            created,
            expires,
            covered_components: components,
        },
        base,
        signature: base64(signature)?,
    })
}
