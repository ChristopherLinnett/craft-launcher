//! The team password, and the sessions it opens.
//!
//! The password is stored as an Argon2 hash in `<data>/config.json`, next to the key that signs
//! session cookies. A cookie holds its expiry and an HMAC of it, so the server keeps no session
//! list; setting a new password makes a new key, which ends every session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use artcraft_launcher_native::http::hex;
use artcraft_launcher_native::paths::write_atomic;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// The session cookie's name.
pub const COOKIE: &str = "artcraft_session";
/// How long a sign-in lasts.
const SESSION_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Wrong passwords one client may try per [`FAILURE_WINDOW`], and all clients together.
const MAX_FAILURES: usize = 10;
const MAX_FAILURES_ALL: usize = 100;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// What the cookie's HMAC covers besides the expiry, so the key signs nothing else by accident.
const SESSION_LABEL: &[u8] = b"artcraft-launcher-host session ";

#[derive(Serialize, Deserialize)]
struct ConfigFile {
    /// Argon2 PHC string.
    password: String,
    /// Hex.
    cookie_key: String,
}

pub struct Auth {
    password: String,
    key: Vec<u8>,
    /// Recent wrong passwords per client.
    failures: Mutex<HashMap<String, Vec<Instant>>>,
}

/// What a sign-in attempt came to.
#[derive(Debug, PartialEq, Eq)]
pub enum SignIn {
    /// The cookie value that opens a session.
    Ok(String),
    Wrong,
    /// Too many wrong passwords lately: not even checked.
    Throttled,
}

fn config_path(data: &Path) -> PathBuf {
    data.join("config.json")
}

impl Auth {
    pub fn load(data: &Path) -> Result<Self, String> {
        let path = config_path(data);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!("couldn't read {} ({e}). Set a password first: artcraft-launcher-host set-password --data {}", path.display(), data.display())
        })?;
        let file: ConfigFile = serde_json::from_str(&text).map_err(|e| format!("{} is unreadable ({e})", path.display()))?;
        PasswordHash::new(&file.password).map_err(|e| format!("{} has no usable password hash ({e})", path.display()))?;
        let key = from_hex(&file.cookie_key).filter(|k| k.len() >= 32).ok_or_else(|| format!("{} has no usable cookie key", path.display()))?;
        Ok(Self { password: file.password, key, failures: Mutex::default() })
    }

    /// Sets the team password and a new cookie key, which ends every session.
    pub fn set_password(data: &Path, password: &str) -> Result<(), String> {
        if password.chars().count() < 8 {
            return Err("use a password of at least 8 characters".into());
        }
        let salt = SaltString::encode_b64(&random::<16>()?).map_err(|e| format!("couldn't make a salt ({e})"))?;
        let hash = Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| format!("couldn't hash the password ({e})"))?;
        let file = ConfigFile { password: hash.to_string(), cookie_key: hex(&random::<32>()?) };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        // A fresh temporary file is private to its owner (0600), and the rename keeps that.
        write_atomic(&config_path(data), &json)
    }

    /// Checks `password` for `client` (an address). `now` is Unix seconds.
    pub fn sign_in(&self, client: &str, password: &str, now: i64) -> SignIn {
        {
            let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            failures.retain(|_, times| {
                times.retain(|t| t.elapsed() < FAILURE_WINDOW);
                !times.is_empty()
            });
            let all: usize = failures.values().map(Vec::len).sum();
            if all >= MAX_FAILURES_ALL || failures.get(client).is_some_and(|t| t.len() >= MAX_FAILURES) {
                return SignIn::Throttled;
            }
        }
        let right = PasswordHash::new(&self.password).is_ok_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok());
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if right {
            failures.remove(client);
            let expires = now.saturating_add(SESSION_SECONDS);
            SignIn::Ok(format!("{expires}.{}", hex(&self.mac(expires).unwrap_or_default())))
        } else {
            failures.entry(client.to_string()).or_default().push(Instant::now());
            SignIn::Wrong
        }
    }

    /// Whether a session cookie's value is one this server signed and still current.
    pub fn valid(&self, cookie: &str, now: i64) -> bool {
        let Some((expires, mac)) = cookie.split_once('.') else { return false };
        let (Ok(expires), Some(mac)) = (expires.parse::<i64>(), from_hex(mac)) else { return false };
        if expires <= now || expires > now.saturating_add(SESSION_SECONDS + 60) {
            return false;
        }
        self.hmac(expires).is_some_and(|m| m.verify_slice(&mac).is_ok())
    }

    fn hmac(&self, expires: i64) -> Option<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).ok()?;
        mac.update(SESSION_LABEL);
        mac.update(expires.to_string().as_bytes());
        Some(mac)
    }

    fn mac(&self, expires: i64) -> Option<Vec<u8>> {
        self.hmac(expires).map(|m| m.finalize().into_bytes().to_vec())
    }
}

/// The `Set-Cookie` value that opens a session.
pub fn session_cookie(value: &str) -> String {
    format!("{COOKIE}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={SESSION_SECONDS}")
}

/// The `Set-Cookie` value that ends one.
pub fn ended_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// The session cookie's value in a `Cookie` header.
pub fn cookie_in(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| pair.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random numbers from the system ({e})"))?;
    Ok(bytes)
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| text.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> (tempfile::TempDir, Auth) {
        let dir = tempfile::tempdir().unwrap();
        Auth::set_password(dir.path(), "correct horse").unwrap();
        let auth = Auth::load(dir.path()).unwrap();
        (dir, auth)
    }

    #[test]
    fn the_password_opens_a_session_that_ends_on_time() {
        let (_dir, auth) = auth();
        let SignIn::Ok(cookie) = auth.sign_in("a", "correct horse", 1_000) else { panic!("refused") };
        assert!(auth.valid(&cookie, 1_000));
        assert!(auth.valid(&cookie, 1_000 + SESSION_SECONDS - 1));
        assert!(!auth.valid(&cookie, 1_000 + SESSION_SECONDS));
        assert_eq!(cookie_in(&format!("x=1; {COOKIE}={cookie}; y=2")), Some(cookie.as_str()));
    }

    #[test]
    fn forged_and_altered_cookies_are_refused() {
        let (_dir, auth) = auth();
        let SignIn::Ok(cookie) = auth.sign_in("a", "correct horse", 1_000) else { panic!("refused") };
        let (expires, mac) = cookie.split_once('.').unwrap();
        let later: i64 = expires.parse::<i64>().unwrap() + 1;
        for bad in [format!("{later}.{mac}"), format!("{expires}.00"), "nonsense".to_string(), String::new()] {
            assert!(!auth.valid(&bad, 1_000), "{bad}");
        }
        // A new password ends the session.
        let dir = tempfile::tempdir().unwrap();
        Auth::set_password(dir.path(), "another password").unwrap();
        assert!(!Auth::load(dir.path()).unwrap().valid(&cookie, 1_000));
    }

    #[test]
    fn wrong_passwords_are_throttled() {
        let (_dir, auth) = auth();
        for _ in 0..MAX_FAILURES {
            assert_eq!(auth.sign_in("a", "guess", 1), SignIn::Wrong);
        }
        // Even the right password waits now, for that client only.
        assert_eq!(auth.sign_in("a", "correct horse", 1), SignIn::Throttled);
        assert!(matches!(auth.sign_in("b", "correct horse", 1), SignIn::Ok(_)));
    }

    #[test]
    fn short_passwords_and_missing_config_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Auth::set_password(dir.path(), "short").is_err());
        assert!(Auth::load(dir.path()).err().is_some_and(|e| e.contains("set-password")));
    }
}
