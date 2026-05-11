use crate::config::Account;
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::http::HeaderMap;
use rand::RngCore;

pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::from("cc_");
    for b in &bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn hash_token(token: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(token.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2: {}", e))?
        .to_string())
}

pub fn verify_token(token: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok()
}

pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(s) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(t) = s.strip_prefix("Bearer ") {
            return Some(t.to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

pub fn authenticate<'a>(
    accounts: &'a [Account],
    headers: &HeaderMap,
) -> Result<&'a Account, &'static str> {
    let token = extract_token(headers).ok_or("missing token")?;
    for a in accounts {
        if verify_token(&token, &a.token_hash) {
            return Ok(a);
        }
    }
    Err("invalid token")
}
