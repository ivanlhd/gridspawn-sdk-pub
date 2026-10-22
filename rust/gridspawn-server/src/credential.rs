use std::{fmt, path::Path, sync::Arc};

use reqwest::header::HeaderValue;
use zeroize::Zeroizing;

use crate::{error::Error, types::token_char};

const PREFIX: &str = "gsc1.";

/// A server credential, `gsc1.<credential id>.<43-character secret>`, made in
/// the Dev Hub (Game servers → New server key) or by an operator.
///
/// `Debug` and `Display` print `gsc1.<id>.****`; the secret is zeroized when
/// the credential is dropped and never logged. Keep it on the server only.
pub struct Credential {
    value: Zeroizing<String>,
    id_end: usize,
}

impl Credential {
    pub fn parse(value: impl Into<String>) -> Result<Self, Error> {
        let raw = Zeroizing::new(value.into());
        let value = Zeroizing::new(raw.trim().to_owned());
        let invalid = || Error::InvalidInput {
            field: "credential",
            reason: "expected gsc1.<id>.<43-character secret>",
        };
        let rest = value.strip_prefix(PREFIX).ok_or_else(invalid)?;
        let (id, secret) = rest.split_once('.').ok_or_else(invalid)?;
        let id_ok = (3..=128).contains(&id.len()) && id.bytes().all(token_char);
        let secret_ok = secret.len() == 43 && secret.bytes().all(token_char);
        if !id_ok || !secret_ok {
            return Err(invalid());
        }
        let id_end = PREFIX.len() + id.len();
        Ok(Self { value, id_end })
    }

    /// Reads the credential from a file (surrounding whitespace is ignored).
    /// On Unix the file must not be readable by group or others, like ssh
    /// private keys; fix it with `chmod 600 <path>`, or use
    /// [`Credential::from_file_unchecked`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|error| Error::CredentialUnreadable(Arc::new(error)))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(Error::CredentialFileTooOpen {
                    path: path.display().to_string(),
                });
            }
        }
        Self::from_file_unchecked(path)
    }

    /// Like [`Credential::from_file`], without the permission check.
    pub fn from_file_unchecked(path: impl AsRef<Path>) -> Result<Self, Error> {
        let contents = Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|error| Error::CredentialUnreadable(Arc::new(error)))?,
        );
        Self::parse(contents.trim().to_owned())
    }

    /// Reads the environment variable `name` (for example `GRIDSPAWN_SERVER_CREDENTIAL`).
    pub fn from_env(name: &str) -> Result<Self, Error> {
        let value = std::env::var(name).map_err(|_| Error::InvalidInput {
            field: "credential",
            reason: "the environment variable is not set or not UTF-8",
        })?;
        Self::parse(value)
    }

    /// The credential id (not secret), for logs.
    pub fn id(&self) -> &str {
        &self.value[PREFIX.len()..self.id_end]
    }

    pub(crate) fn authorization(&self) -> HeaderValue {
        let header = Zeroizing::new(format!("GridspawnServer {}", self.value.as_str()));
        let mut value = HeaderValue::from_str(&header).expect("credential is ASCII");
        value.set_sensitive(true);
        value
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credential({PREFIX}{}.****)", self.id())
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}.****", self.id())
    }
}
