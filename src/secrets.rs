//! Sealed credential store. Secret VALUES must never sit in plaintext on disk
//! and must never be logged. This module is the only place secrets are written
//! to or read from durable storage.
//!
//! # Backend selection (chosen once at boot, logged by NAME only)
//!
//! 1. **OS keychain via the `keyring` crate** — preferred on a developer's
//!    machine. On macOS this is the login Keychain (the `apple-native` backend,
//!    the only keyring backend we compile in). Values live in the OS secure
//!    store; only an index of credential *names* is written to the jail so we
//!    know what to rehydrate at boot (keyring has no "enumerate entries" API).
//!
//! 2. **Encrypted file (XChaCha20-Poly1305)** — the fallback used whenever the
//!    keychain is unavailable. This is the path a **headless Raspberry Pi**
//!    takes: a desktop secret-service daemon (gnome-keyring / KWallet over
//!    D-Bus) is usually absent on a server install, and the kernel keyutils
//!    backend is non-persistent across reboots — so on the Pi we deliberately
//!    seal the secrets ourselves. The name→value map is AEAD-encrypted to
//!    `creds_sealed.bin`; the 256-bit master key is a `0600` file (`creds.key`)
//!    inside the jail.
//!
//! ## Tradeoff (documented per the task)
//!
//! The keychain backend is the strongest at-rest posture (the OS guards the key
//! material and can require a login/unlock), but it is desktop-bound and not
//! reliably present on a headless target. The encrypted-file backend is fully
//! portable and needs no system daemon or C libraries — its security reduces to
//! filesystem protection of the `creds.key` master key (mode 0600, owned by the
//! kerneld user, inside the sandbox jail). That is the right tradeoff for a
//! single-user appliance like the Pi: an attacker who can already read arbitrary
//! files as the daemon user could read the live process memory anyway. We do NOT
//! invent a passphrase-derived key here because a headless boot has no operator
//! to type one; binding the key to the jail's filesystem ACL is the honest,
//! portable floor. Hardening to a TPM / Pi secure element is a future step.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

const SERVICE: &str = "webos-kerneld";
const KEY_LEN: usize = 32; // XChaCha20-Poly1305 key
const NONCE_LEN: usize = 24; // XChaCha20-Poly1305 (X) nonce

fn jail() -> PathBuf {
    crate::caps::root_dir()
}
fn index_path() -> PathBuf {
    jail().join("creds_index.json")
}
fn sealed_path() -> PathBuf {
    jail().join("creds_sealed.bin")
}
fn key_path() -> PathBuf {
    jail().join("creds.key")
}

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    Keychain,
    SealedFile,
}

/// The durable secret store. Holds only the chosen backend (and, for the file
/// backend, the loaded master key) — never the secret values themselves, which
/// live in `AppState.creds` while the daemon runs.
pub struct SecretStore {
    backend: Backend,
    key: Option<[u8; KEY_LEN]>, // present only for SealedFile
}

impl SecretStore {
    /// Pick a backend once. Probe the OS keychain with a throwaway round-trip;
    /// if that fails for any reason, fall back to the sealed file. Logs the
    /// chosen backend by NAME only — never a value.
    ///
    /// `WEBOS_SECRETS_BACKEND=file` forces the encrypted-file path even when a
    /// keychain is present — useful for testing the Pi/headless path on a dev
    /// machine, or for an operator who prefers the self-sealed store.
    pub fn open() -> Self {
        let forced = std::env::var("WEBOS_SECRETS_BACKEND").unwrap_or_default();
        if forced != "file" && keychain_available() {
            tracing::info!("secret store: OS keychain backend (keyring)");
            return SecretStore { backend: Backend::Keychain, key: None };
        }
        let key = load_or_create_master_key();
        tracing::info!("secret store: encrypted-file backend (XChaCha20-Poly1305)");
        SecretStore { backend: Backend::SealedFile, key: Some(key) }
    }

    /// Rehydrate all stored secrets into a name→value map at boot.
    pub fn load_all(&self) -> HashMap<String, String> {
        match self.backend {
            Backend::Keychain => self.load_all_keychain(),
            Backend::SealedFile => self.load_all_sealed(),
        }
    }

    /// Persist a single credential. The in-memory map in AppState is the source
    /// of truth that we serialize from for the file backend; `live` is that map
    /// AFTER the caller inserted/updated this name.
    pub fn set(&self, name: &str, value: &str, live: &HashMap<String, String>) -> Result<(), String> {
        match self.backend {
            Backend::Keychain => {
                let entry = keyring::Entry::new(SERVICE, name).map_err(redact_kr)?;
                entry.set_password(value).map_err(redact_kr)?;
                self.write_index(live);
                Ok(())
            }
            Backend::SealedFile => self.reseal(live),
        }
    }

    /// Remove a credential. `live` is the map AFTER the caller removed this name.
    pub fn delete(&self, name: &str, live: &HashMap<String, String>) -> Result<(), String> {
        match self.backend {
            Backend::Keychain => {
                if let Ok(entry) = keyring::Entry::new(SERVICE, name) {
                    // NoEntry is fine — deleting something already absent.
                    match entry.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(e) => return Err(redact_kr(e)),
                    }
                }
                self.write_index(live);
                Ok(())
            }
            Backend::SealedFile => self.reseal(live),
        }
    }

    // ── keychain backend ──────────────────────────────────────────────────────

    fn load_all_keychain(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for name in self.read_index() {
            match keyring::Entry::new(SERVICE, &name).and_then(|e| e.get_password()) {
                Ok(v) => {
                    out.insert(name, v);
                }
                Err(keyring::Error::NoEntry) => {
                    // Index drifted from the keychain (e.g. entry removed out of
                    // band). Skip — log the NAME only.
                    tracing::warn!("secret '{name}' indexed but absent from keychain; skipping");
                }
                Err(e) => tracing::warn!("could not load secret '{name}': {}", redact_kr(e)),
            }
        }
        out
    }

    /// Names-only index so we know what to rehydrate (keyring can't enumerate).
    fn write_index(&self, live: &HashMap<String, String>) {
        let mut names: Vec<&String> = live.keys().collect();
        names.sort();
        let body = serde_json::json!({ "names": names });
        if let Err(e) = std::fs::write(index_path(), body.to_string()) {
            tracing::warn!("could not write creds index: {e}");
        }
    }

    fn read_index(&self) -> Vec<String> {
        let txt = match std::fs::read_to_string(index_path()) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        serde_json::from_str::<Value>(&txt)
            .ok()
            .and_then(|v| v.get("names").and_then(|n| n.as_array()).cloned())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    // ── sealed-file backend ─────────────────────────────────────────────────────

    fn load_all_sealed(&self) -> HashMap<String, String> {
        let key = match self.key {
            Some(k) => k,
            None => return HashMap::new(),
        };
        let blob = match std::fs::read(sealed_path()) {
            Ok(b) => b,
            Err(_) => return HashMap::new(), // first boot, nothing sealed yet
        };
        if blob.len() < NONCE_LEN {
            tracing::warn!("sealed creds file too short; ignoring");
            return HashMap::new();
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        let nonce = XNonce::from_slice(nonce_bytes);
        match cipher.decrypt(nonce, ciphertext) {
            Ok(plain) => serde_json::from_slice::<HashMap<String, String>>(&plain).unwrap_or_else(|e| {
                tracing::warn!("sealed creds decrypted but malformed: {e}");
                HashMap::new()
            }),
            Err(_) => {
                // AEAD auth failure — wrong key or tampering. Never log details
                // that could leak material; surface only the fact.
                tracing::error!("sealed creds failed authentication (key mismatch or tampering); ignoring");
                HashMap::new()
            }
        }
    }

    /// Encrypt the FULL live map and atomically replace the sealed file.
    fn reseal(&self, live: &HashMap<String, String>) -> Result<(), String> {
        let key = self.key.ok_or("sealed backend missing master key")?;
        let plain = serde_json::to_vec(live).map_err(|e| format!("serialize secrets: {e}"))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        let nonce = XNonce::from_slice(&nonce_bytes);
        // Never include the AEAD error's Display (defensive — it carries no
        // plaintext, but we keep secrets-adjacent errors generic).
        let ciphertext = cipher.encrypt(nonce, plain.as_ref()).map_err(|_| "seal failed".to_string())?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        atomic_write_0600(&sealed_path(), &blob)
    }
}

// ── keychain probe ──────────────────────────────────────────────────────────

/// Round-trip a throwaway secret to decide whether the keychain is usable in
/// this environment (headless Linux usually has no secret-service → false).
fn keychain_available() -> bool {
    const PROBE: &str = "__webos_probe__";
    let entry = match keyring::Entry::new(SERVICE, PROBE) {
        Ok(e) => e,
        Err(_) => return false,
    };
    if entry.set_password("ok").is_err() {
        return false;
    }
    let ok = matches!(entry.get_password().as_deref(), Ok("ok"));
    let _ = entry.delete_credential();
    ok
}

// ── master key for the sealed-file backend ──────────────────────────────────

/// Load the 0600 master key, generating one on first run. The key file is the
/// crux of the file-backend's at-rest protection, so it is created with 0600
/// and never logged.
fn load_or_create_master_key() -> [u8; KEY_LEN] {
    let path = key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == KEY_LEN {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&bytes);
            return k;
        }
        tracing::warn!("creds.key wrong length; regenerating (existing sealed secrets will be unreadable)");
    }
    let mut k = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut k);
    if let Err(e) = atomic_write_0600(&path, &k) {
        tracing::error!("could not persist creds master key: {e}");
    }
    k
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Write bytes with mode 0600 (owner read/write only). On non-unix this falls
/// back to a plain write (the keychain backend is used on those platforms).
fn atomic_write_0600(path: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("open temp: {e}"))?;
            f.write_all(bytes).map_err(|e| format!("write temp: {e}"))?;
            f.sync_all().map_err(|e| format!("sync temp: {e}"))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, bytes).map_err(|e| format!("write temp: {e}"))?;
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Map a keyring error to a string with NO secret material. keyring errors
/// describe storage/access state, not values, but we keep this centralized so
/// every error surfaced from this module is reviewed for leakage.
fn redact_kr(e: keyring::Error) -> String {
    match e {
        keyring::Error::NoEntry => "no such credential".into(),
        keyring::Error::NoStorageAccess(_) => "secure storage unavailable".into(),
        keyring::Error::PlatformFailure(_) => "secure storage platform error".into(),
        keyring::Error::Ambiguous(_) => "ambiguous credential match".into(),
        _ => "secret store error".into(),
    }
}
