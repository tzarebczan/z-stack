//! Encrypted seed storage + optional OS credential unlock (Windows Hello / Credential Locker).
//!
//! Layout:
//! - `seed.enc` — Argon2id + ChaCha20-Poly1305 ciphertext of the BIP-39 mnemonic
//! - Optional Windows Credential Manager entry holding the same mnemonic for biometric/OS unlock
//!
//! Never write a plaintext `mnemonic.txt`.

use crate::error::{EngineError, Result};
use argon2::{
    password_hash::{PasswordHasher, SaltString},
    Argon2,
};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use rand::TryRng;
use secrecy::{ExposeSecret, SecretString};
use std::path::{Path, PathBuf};
use zeroize::Zeroize;

const MAGIC: &[u8; 8] = b"ZSEED001";
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;
const KEYRING_SERVICE: &str = "z-stack-wallet";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedUnlock {
    Passphrase,
    OsKeychain,
}

/// When the spending seed is requested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnlockPolicy {
    /// Unlock once; keep in memory until the process exits.
    #[default]
    Session,
    /// Prompt (Hello / passphrase / paste) on every send or shield.
    EachSpend,
    /// Unlock from the OS keychain as soon as the wallet opens.
    Always,
}

impl UnlockPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::EachSpend => "each-spend",
            Self::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "session" => Some(Self::Session),
            "each" | "each-spend" | "ask" => Some(Self::EachSpend),
            "always" | "startup" => Some(Self::Always),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "Session only",
            Self::EachSpend => "Ask each time",
            Self::Always => "Unlock at startup",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Session => "Unlock once; keep the seed in memory until you quit.",
            Self::EachSpend => {
                "Confirm in the app on every send or shield (paste or saved OS seed). Auto-shield is off."
            }
            Self::Always => "Unlock from the OS keychain when the wallet opens, and stay unlocked.",
        }
    }
}

pub struct SeedStore {
    root: PathBuf,
}

impl SeedStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn enc_path(&self) -> PathBuf {
        self.root.join("seed.enc")
    }

    fn keyring_user(&self) -> String {
        // Stable id per wallet directory.
        format!("wallet:{}", self.root.display())
    }

    pub fn exists(&self) -> bool {
        self.enc_path().exists() || self.os_unlock_present()
    }

    pub fn has_encrypted(&self) -> bool {
        self.enc_path().exists()
    }

    pub fn os_unlock_present(&self) -> bool {
        self.windows_credential_present()
    }

    pub fn os_unlock_label() -> &'static str {
        if cfg!(windows) {
            "Windows Hello / Credential Manager"
        } else if cfg!(target_os = "macos") {
            "Keychain (Touch ID if this Mac asks)"
        } else {
            "system keyring (Secret Service)"
        }
    }

    fn windows_credential_present(&self) -> bool {
        #[cfg(feature = "seed-store")]
        {
            keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user())
                .ok()
                .and_then(|e| e.get_password().ok())
                .is_some()
        }
        #[cfg(not(feature = "seed-store"))]
        {
            false
        }
    }

    /// Persist mnemonic. Prefer passphrase encryption; optionally also store in OS keyring.
    pub fn save(
        &self,
        mnemonic: &str,
        passphrase: Option<&str>,
        windows_credential: bool,
    ) -> Result<()> {
        if passphrase.is_none() && !windows_credential {
            return Err(EngineError::SeedLocked);
        }
        std::fs::create_dir_all(&self.root)?;

        if let Some(pass) = passphrase {
            self.write_encrypted(mnemonic, pass)?;
        }
        if windows_credential {
            self.save_windows_credential(mnemonic)?;
        }
        // Remove any legacy plaintext file.
        let legacy = self.root.join("mnemonic.txt");
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }
        Ok(())
    }

    fn write_encrypted(&self, mnemonic: &str, passphrase: &str) -> Result<()> {
        let mut salt_bytes = [0u8; SALT_LEN];
        UnwrapErr(SysRng)
            .try_fill_bytes(&mut salt_bytes)
            .map_err(|e| EngineError::Message(format!("rng: {e}")))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| EngineError::Message(format!("salt: {e}")))?;

        let argon = Argon2::default();
        let hash = argon
            .hash_password(passphrase.as_bytes(), &salt)
            .map_err(|e| EngineError::Message(format!("argon2: {e}")))?;
        let hash_bytes = hash
            .hash
            .ok_or_else(|| EngineError::Message("argon2 missing hash".into()))?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash_bytes.as_bytes()[..32]);

        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|e| EngineError::Message(format!("cipher: {e}")))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        UnwrapErr(SysRng)
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|e| EngineError::Message(format!("rng: {e}")))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, mnemonic.as_bytes())
            .map_err(|e| EngineError::Message(format!("encrypt: {e}")))?;

        key.zeroize();

        let mut out = Vec::with_capacity(8 + SALT_LEN + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&salt_bytes);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        // Re-saving (attach seed, change unlock) must never tear the only copy.
        super::replace_file(&self.enc_path(), &out, harden_file_acl)
    }

    fn save_windows_credential(&self, mnemonic: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user())
            .map_err(|e| EngineError::Message(format!("keyring: {e}")))?;
        entry.set_password(mnemonic).map_err(|e| {
            EngineError::Message(format!(
                "OS keychain write failed ({}): {e}",
                Self::os_unlock_label()
            ))
        })?;
        Ok(())
    }

    /// Load mnemonic as a String (caller must not log it).
    pub fn load_words(
        &self,
        passphrase: Option<&str>,
        prefer_windows_credential: bool,
    ) -> Result<String> {
        Ok(self
            .load(passphrase, prefer_windows_credential)?
            .expose_secret()
            .clone())
    }

    /// Load mnemonic using passphrase and/or Windows credential.
    pub fn load(
        &self,
        passphrase: Option<&str>,
        prefer_windows_credential: bool,
    ) -> Result<SecretString> {
        if prefer_windows_credential || passphrase.is_none() {
            if let Ok(m) = self.load_windows_credential() {
                return Ok(m);
            }
            if passphrase.is_none() {
                return Err(EngineError::SeedLocked);
            }
        }
        let pass = passphrase.ok_or(EngineError::SeedLocked)?;
        self.read_encrypted(pass)
    }

    fn load_windows_credential(&self) -> Result<SecretString> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user())
            .map_err(|e| EngineError::Message(format!("keyring: {e}")))?;
        let pw = entry.get_password().map_err(|_| EngineError::SeedLocked)?;
        Ok(SecretString::new(pw))
    }

    fn read_encrypted(&self, passphrase: &str) -> Result<SecretString> {
        let bytes = std::fs::read(self.enc_path()).map_err(|_| EngineError::SeedLocked)?;
        if bytes.len() < 8 + SALT_LEN + NONCE_LEN + 16 || &bytes[..8] != MAGIC {
            return Err(EngineError::SeedDecryptFailed);
        }
        let salt_bytes = &bytes[8..8 + SALT_LEN];
        let nonce_bytes = &bytes[8 + SALT_LEN..8 + SALT_LEN + NONCE_LEN];
        let ciphertext = &bytes[8 + SALT_LEN + NONCE_LEN..];

        let salt = SaltString::encode_b64(salt_bytes)
            .map_err(|e| EngineError::Message(format!("salt: {e}")))?;
        let argon = Argon2::default();
        let hash = argon
            .hash_password(passphrase.as_bytes(), &salt)
            .map_err(|_| EngineError::SeedDecryptFailed)?;
        let hash_bytes = hash.hash.ok_or(EngineError::SeedDecryptFailed)?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash_bytes.as_bytes()[..32]);

        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|e| EngineError::Message(format!("cipher: {e}")))?;
        key.zeroize();
        let nonce = Nonce::from_slice(nonce_bytes);
        let plain = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| EngineError::SeedDecryptFailed)?;
        let s = String::from_utf8(plain).map_err(|_| EngineError::SeedDecryptFailed)?;
        Ok(SecretString::new(s))
    }
}

fn harden_file_acl(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        // Best-effort: restrict to current user via icacls.
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{}:(R,W)", whoami_user()))
            .output();
    }
    let _ = path;
    Ok(())
}

#[cfg(windows)]
fn whoami_user() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "%USERNAME%".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_passphrase() {
        let dir = tempdir().unwrap();
        let store = SeedStore::new(dir.path());
        store
            .save("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", Some("test-pass"), false)
            .unwrap();
        assert!(!dir.path().join("mnemonic.txt").exists());
        let got = store.load(Some("test-pass"), false).unwrap();
        assert!(got.expose_secret().starts_with("abandon"));
        assert!(store.load(Some("wrong"), false).is_err());
    }
}
