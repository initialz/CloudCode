use crate::db::{Db, DbAccount};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::http::HeaderMap;
use rand::RngCore;

pub fn generate_token() -> String {
    generate_token_with_prefix("cc_")
}

pub fn generate_agent_token() -> String {
    generate_token_with_prefix("ag_")
}

pub fn generate_admin_token() -> String {
    generate_token_with_prefix("ad_")
}

pub fn generate_session_id() -> String {
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn generate_token_with_prefix(prefix: &str) -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::from(prefix);
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

/// Look up a plaintext token against every active account in the db.
/// O(N) on accounts (each row needs an argon2 verify) but N is small
/// — hubs in the wild have one account per developer.
pub async fn authenticate(db: &Db, headers: &HeaderMap) -> Result<DbAccount, &'static str> {
    let token = extract_token(headers).ok_or("missing token")?;
    let accounts = db.list_accounts().await.map_err(|_| "db error")?;
    for a in accounts {
        if a.disabled {
            continue;
        }
        if verify_token(&token, &a.token_hash) {
            return Ok(a);
        }
    }
    Err("invalid token")
}
