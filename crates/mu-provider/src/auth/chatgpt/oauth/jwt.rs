//! ID token verification: RS256 against the issuer's JWKS, then the claims the protocol requires.

use super::LEEWAY;
use crate::auth::chatgpt::Endpoints;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

pub(super) struct Claims {
    pub sub: String,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

pub(super) async fn verify_id_token(
    ep: &Endpoints,
    token: &str,
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<Claims> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        bail!("ChatGPT's ID token is not a JWT");
    };
    let header: Header = serde_json::from_slice(&b64(head)?).context("ChatGPT's ID token header is not JSON")?;
    if header.alg != "RS256" {
        bail!("ChatGPT's ID token is not signed with RS256");
    }
    let kid = header.kid.context("ChatGPT's ID token has no kid")?;
    let key = signing_key(ep, &kid).await?;
    let (Some(n), Some(e)) = (key.n, key.e) else {
        bail!("ChatGPT's signing key is incomplete");
    };
    let signed = format!("{head}.{body}");
    RsaPublicKeyComponents { n: b64(&n)?, e: b64(&e)? }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &b64(sig)?)
        .map_err(|_| anyhow!("ChatGPT's ID token signature is invalid"))?;

    let claims: Value = serde_json::from_slice(&b64(body)?).context("ChatGPT's ID token payload is not JSON")?;
    if claims["iss"].as_str() != Some(ep.issuer.as_str()) {
        bail!("ChatGPT's ID token was issued by another issuer");
    }
    let audience = match &claims["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => a.iter().any(|v| v.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience {
        bail!("ChatGPT's ID token is for another client");
    }
    let exp = claims["exp"].as_i64().context("ChatGPT's ID token has no expiry")?;
    if now >= exp + LEEWAY {
        bail!("ChatGPT's ID token has expired");
    }
    if claims["nonce"].as_str() != Some(nonce) {
        bail!("ChatGPT's ID token nonce does not match this sign-in");
    }
    let sub = claims["sub"].as_str().filter(|s| !s.is_empty()).context("ChatGPT's ID token has no subject")?;
    let email = claims["email"].as_str().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty());
    Ok(Claims { sub: sub.to_string(), email })
}

async fn signing_key(ep: &Endpoints, kid: &str) -> Result<Jwk> {
    let client = crate::net::http_client().context("building the HTTP client")?;
    let resp = client
        .get(&ep.jwks)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("fetching ChatGPT's signing keys")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("fetching ChatGPT's signing keys failed (HTTP {})", status.as_u16());
    }
    let text = resp.text().await.context("reading ChatGPT's signing keys")?;
    let set: Jwks = serde_json::from_str(&text).context("ChatGPT's signing keys are not JSON")?;
    set.keys
        .into_iter()
        .find(|k| k.kid.as_deref() == Some(kid) && k.kty.as_deref() == Some("RSA"))
        .context("no ChatGPT signing key matches the ID token")
}

fn b64(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).context("a JWT part is not base64url")
}
