//! Bearer and basic authentication against the two configured tokens.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::Token;

/// Who made a request, attached to the request's extensions once the token
/// checks out. Every handler and tool reads it from there.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub can_write: bool,
}

pub struct Tokens {
    read_write: Known,
    read_only: Known,
}

struct Known {
    digest: [u8; 32],
    name: String,
}

impl Known {
    fn new(token: &Token) -> Self {
        Self {
            digest: Sha256::digest(token.secret.as_bytes()).into(),
            name: token.name.clone(),
        }
    }
}

impl Tokens {
    pub fn new(read_write: &Token, read_only: &Token) -> Self {
        Self {
            read_write: Known::new(read_write),
            read_only: Known::new(read_only),
        }
    }

    /// Compares digests rather than the tokens themselves, so the time taken
    /// depends neither on how much of a token matches nor on its length, and
    /// both comparisons always run.
    pub fn identify(&self, presented: &str) -> Option<Caller> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let is_rw = bool::from(digest.ct_eq(&self.read_write.digest));
        let is_ro = bool::from(digest.ct_eq(&self.read_only.digest));
        match (is_rw, is_ro) {
            (true, _) => Some(Caller {
                name: self.read_write.name.clone(),
                can_write: true,
            }),
            (false, true) => Some(Caller {
                name: self.read_only.name.clone(),
                can_write: false,
            }),
            (false, false) => None,
        }
    }
}

/// Requires `Authorization: Bearer <token>` with either token.
pub async fn bearer(State(tokens): State<Arc<Tokens>>, mut request: Request, next: Next) -> Response {
    let caller = bearer_token(request.headers()).and_then(|token| tokens.identify(token));
    match caller {
        Some(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            "a valid bearer token is required",
        )
            .into_response(),
    }
}

/// Requires HTTP basic auth whose password is the read and write token. The
/// user name is ignored.
pub async fn basic_read_write(State(tokens): State<Arc<Tokens>>, mut request: Request, next: Next) -> Response {
    let caller = basic_password(request.headers())
        .and_then(|password| tokens.identify(&password))
        .filter(|caller| caller.can_write);
    match caller {
        Some(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"house\", charset=\"UTF-8\""),
            )],
            "the read and write token is required as the password",
        )
            .into_response(),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

fn basic_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (_user, password) = decoded.split_once(':')?;
    Some(password.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Tokens {
        Tokens::new(
            &Token {
                name: "writer".into(),
                secret: "w".repeat(40),
            },
            &Token {
                name: "reader".into(),
                secret: "r".repeat(40),
            },
        )
    }

    #[test]
    fn each_token_maps_to_its_own_name_and_access() {
        let tokens = tokens();
        let writer = tokens.identify(&"w".repeat(40)).expect("writer");
        assert_eq!((writer.name.as_str(), writer.can_write), ("writer", true));
        let reader = tokens.identify(&"r".repeat(40)).expect("reader");
        assert_eq!((reader.name.as_str(), reader.can_write), ("reader", false));
    }

    #[test]
    fn near_misses_are_refused() {
        let tokens = tokens();
        assert!(tokens.identify(&"w".repeat(39)).is_none());
        assert!(tokens.identify(&"w".repeat(41)).is_none());
        assert!(tokens.identify("").is_none());
    }

    #[test]
    fn basic_auth_password_is_extracted() {
        let mut headers = HeaderMap::new();
        let encoded = STANDARD.encode("anyone:se:cret");
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {encoded}").parse().expect("header"),
        );
        assert_eq!(basic_password(&headers).as_deref(), Some("se:cret"));
    }
}
