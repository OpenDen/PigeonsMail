//! OS keyring access for the database key and per-account secrets.
use anyhow::{anyhow, Context, Result};
use keyring::Entry;
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

const SERVICE: &str = "pigeonmail";
/// Service name used before the rename to PigeonMail; read only to migrate existing secrets.
const LEGACY_SERVICE: &str = "mailthin";
const DB_KEY_USER: &str = "database-key";

/// Returns the 256-bit database key (64 hex chars), creating and persisting it on first use.
/// A key saved under the legacy service is reused so an existing database stays readable.
pub fn get_or_create_db_key() -> Result<Zeroizing<String>> {
    let entry = Entry::new(SERVICE, DB_KEY_USER).context("keyring entry")?;
    match entry.get_password() {
        Ok(existing) => Ok(Zeroizing::new(existing)),
        Err(keyring::Error::NoEntry) => {
            if let Some(legacy) = read_password(LEGACY_SERVICE, DB_KEY_USER)? {
                entry.set_password(&legacy).context("copy database key to keyring")?;
                return Ok(legacy);
            }
            let mut bytes = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(&mut *bytes);
            let hex_key = Zeroizing::new(hex::encode(&*bytes));
            entry.set_password(&hex_key).context("store database key in keyring")?;
            Ok(hex_key)
        }
        Err(e) => Err(anyhow!("keyring error: {e}")),
    }
}

/// Stores an account secret (app password or OAuth refresh token).
pub fn set_account_secret(email: &str, secret: &str) -> Result<()> {
    Entry::new(SERVICE, &format!("account:{email}"))?
        .set_password(secret)
        .context("store account secret")
}

/// Reads an account secret, copying it from the legacy service the first time it is needed.
pub fn get_account_secret(email: &str) -> Result<Option<Zeroizing<String>>> {
    let user = format!("account:{email}");
    if let Some(secret) = read_password(SERVICE, &user)? {
        return Ok(Some(secret));
    }
    match read_password(LEGACY_SERVICE, &user)? {
        Some(legacy) => {
            Entry::new(SERVICE, &user)?
                .set_password(&legacy)
                .context("copy account secret to keyring")?;
            Ok(Some(legacy))
        }
        None => Ok(None),
    }
}

fn read_password(service: &str, user: &str) -> Result<Option<Zeroizing<String>>> {
    match Entry::new(service, user)?.get_password() {
        Ok(s) => Ok(Some(Zeroizing::new(s))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow!("keyring error: {e}")),
    }
}

pub fn delete_account_secret(email: &str) -> Result<()> {
    match Entry::new(SERVICE, &format!("account:{email}"))?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("keyring error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Touches the real OS credential store, so it is opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn account_secret_roundtrip() {
        let email = "keyring-test@pigeonmail.invalid";
        set_account_secret(email, "s3cret").unwrap();
        assert_eq!(get_account_secret(email).unwrap().unwrap().as_str(), "s3cret");
        delete_account_secret(email).unwrap();
        assert!(get_account_secret(email).unwrap().is_none());
    }
}
