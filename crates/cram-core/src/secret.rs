//! Password handling for encrypted archives.
//!
//! Two rules the whole engine follows:
//!   1. **Passwords are never stored in plain `String`.** They live in [`Secret`], a `Zeroizing`
//!      wrapper that wipes its bytes on drop and refuses to `Debug`/`Display` its contents — so a
//!      password can't leak into a log line, a panic message, or a serialized struct.
//!   2. **Backends never hold the password; they ask for it.** Extraction takes a
//!      [`PasswordProvider`] and calls back *only when it actually meets an encrypted entry or an
//!      encrypted header*. On a wrong password the engine re-asks with `attempt + 1`, so the GUI
//!      can show "wrong password, try again" without restarting the job.
//!
//! Creating an encrypted archive uses [`EncryptSpec`], which carries the user's choices from the
//! two create-dialog forks: the ZIP cipher (AES-256, or the labeled-weak legacy ZipCrypto) and,
//! per archive, whether to encrypt the file listing too ([`HeaderMode`]).

use std::fmt;

use zeroize::Zeroizing;

/// A password held so it is wiped from memory on drop and never printed. Construct with
/// [`Secret::new`]; read the bytes only at the moment of use via [`Secret::expose`].
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(password: impl Into<String>) -> Self {
        Self(Zeroizing::new(password.into()))
    }
    /// Borrow the plaintext — call this as late as possible and don't copy it into an
    /// un-zeroized `String`.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// Never reveal the secret through the usual formatting traits.
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Context handed to a [`PasswordProvider`] so it can prompt intelligently.
#[derive(Debug, Clone)]
pub struct PasswordRequest<'a> {
    /// Archive file name, for the prompt ("Enter password for backup.7z").
    pub archive: &'a str,
    /// The specific entry needing a password, if the format uses per-entry passwords (rare).
    pub entry: Option<&'a str>,
    /// `true` when the password is needed to read the *header/listing* itself (encrypted-names
    /// 7z/RAR/.cram) — the GUI must prompt before it can even show the file tree.
    pub for_header: bool,
    /// 0 on the first ask; incremented after each `WrongPassword` so the UI can say "try again".
    pub attempt: u32,
}

/// Supplies passwords on demand. `Send + Sync` so worker threads share `&dyn PasswordProvider`.
pub trait PasswordProvider: Send + Sync {
    /// Return the password to try, or `None` to give up (→ `PasswordRequired` / `WrongPassword`).
    fn password(&self, req: &PasswordRequest<'_>) -> Option<Secret>;
}

/// Never supplies a password — encrypted archives surface `PasswordRequired` cleanly.
pub struct NoPassword;
impl PasswordProvider for NoPassword {
    fn password(&self, _req: &PasswordRequest<'_>) -> Option<Secret> {
        None
    }
}

/// A single known password (CLI `--password`, or a GUI that pre-collected it). Offered once; on a
/// re-ask (`attempt >= 1`) it returns `None` so a wrong password fails fast instead of looping.
pub struct FixedPassword(pub Secret);
impl PasswordProvider for FixedPassword {
    fn password(&self, req: &PasswordRequest<'_>) -> Option<Secret> {
        (req.attempt == 0).then(|| self.0.clone())
    }
}

/// Wraps any closure (e.g. a GUI prompt) as a provider.
pub struct PromptFn<F>(pub F);
impl<F> PasswordProvider for PromptFn<F>
where
    F: Fn(&PasswordRequest<'_>) -> Option<Secret> + Send + Sync,
{
    fn password(&self, req: &PasswordRequest<'_>) -> Option<Secret> {
        (self.0)(req)
    }
}

/// ZIP encryption method chosen at creation time (fork #1). Other containers are always AES-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZipCipher {
    /// WinZip AES-256 — strong, modern, read by 7-Zip/WinZip/recent Windows.
    #[default]
    Aes256,
    /// Legacy PKWARE ZipCrypto — weak/breakable, offered only for compatibility and surfaced as
    /// such in the UI. Never the default.
    LegacyZipCrypto,
}

/// Whether to encrypt the file listing in addition to file contents (fork #2). Only meaningful for
/// formats that can hide names (7z, `.cram`); ZIP always exposes names, tar-family can't encrypt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderMode {
    /// Contents encrypted, listing browsable without the password.
    #[default]
    ContentsOnly,
    /// Listing encrypted too — the password is required even to see what's inside.
    NamesToo,
}

/// The encryption request for *creating* an archive. Absence of this (`None` in the create options)
/// means "no encryption".
#[derive(Debug, Clone)]
pub struct EncryptSpec {
    pub password: Secret,
    /// ZIP only — ignored by other containers.
    pub zip_cipher: ZipCipher,
    /// 7z / `.cram` only — the per-archive choice from the create dialog.
    pub header: HeaderMode,
}

impl EncryptSpec {
    /// Sensible defaults: AES-256, contents-only. Callers override per the create dialog.
    pub fn new(password: Secret) -> Self {
        Self {
            password,
            zip_cipher: ZipCipher::default(),
            header: HeaderMode::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_does_not_leak_in_debug() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "hunter2");
        assert!(!s.is_empty());
    }

    #[test]
    fn fixed_password_offered_once_then_gives_up() {
        let p = FixedPassword(Secret::new("pw"));
        let req = |attempt| PasswordRequest {
            archive: "a.7z",
            entry: None,
            for_header: false,
            attempt,
        };
        assert_eq!(p.password(&req(0)).unwrap().expose(), "pw");
        assert!(p.password(&req(1)).is_none()); // wrong-password re-ask fails fast
    }

    #[test]
    fn no_password_always_none() {
        let req = PasswordRequest {
            archive: "a.zip",
            entry: None,
            for_header: true,
            attempt: 0,
        };
        assert!(NoPassword.password(&req).is_none());
    }

    #[test]
    fn encrypt_spec_defaults_are_aes_contents_only() {
        let spec = EncryptSpec::new(Secret::new("k"));
        assert_eq!(spec.zip_cipher, ZipCipher::Aes256);
        assert_eq!(spec.header, HeaderMode::ContentsOnly);
    }
}
