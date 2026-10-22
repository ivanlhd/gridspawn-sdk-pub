use std::{fmt, str::FromStr, time::Duration};

use crate::error::Error;

/// Where the game server runs. Each environment has its own server keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

impl FromStr for Environment {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "development" => Ok(Self::Development),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            _ => Err(Error::InvalidInput {
                field: "environment",
                reason: "must be development, staging or production",
            }),
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `Idempotency-Key`: 8–128 characters of `[A-Za-z0-9._:~-]` (the SDK
/// edge's rule, which also satisfies Core's). A call and all its retries send
/// the same key, so the service acts on it once.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        let valid = (8..=128).contains(&value.len())
            && value.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'~' | b'-')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(Error::InvalidInput {
                field: "idempotency key",
                reason: "8-128 characters of A-Z a-z 0-9 . _ : ~ -",
            })
        }
    }

    /// A random key (UUID v4), for calls without a natural one.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How network errors, timeouts and HTTP 5xx answers are retried. 429 and
/// other answers are never retried automatically.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Total attempts including the first (default 3).
    pub max_attempts: u32,
    /// First backoff (default 200 ms); doubles each attempt, ±20 % jitter.
    pub initial_backoff: Duration,
    /// Upper bound for one backoff (default 5 s).
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// One attempt, no retries.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// The wait after failed attempt `attempt` (1-based).
    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        let base = self
            .initial_backoff
            .saturating_mul(1u32 << exponent)
            .min(self.max_backoff);
        // ±20 % jitter from a random byte.
        let byte = uuid::Uuid::new_v4().as_bytes()[0];
        let factor = 0.8 + 0.4 * (f64::from(byte) / 255.0);
        base.mul_f64(factor)
    }
}

/// A module a client ticket can open (02 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Module {
    Presence,
    Voice,
    Interakt,
}

impl Module {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Presence => "presence",
            Self::Voice => "voice",
            Self::Interakt => "interakt",
        }
    }

    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "presence" => Some(Self::Presence),
            "voice" => Some(Self::Voice),
            "interakt" => Some(Self::Interakt),
            _ => None,
        }
    }
}

/// `value` has `min..=max` characters and no control characters.
pub(crate) fn bounded(
    value: &str,
    min: usize,
    max: usize,
    field: &'static str,
) -> Result<(), Error> {
    let count = value.chars().count();
    if count < min || count > max || value.chars().any(char::is_control) {
        return Err(Error::InvalidInput {
            field,
            reason: "length out of range or contains control characters",
        });
    }
    Ok(())
}

/// `[A-Za-z0-9_-]`.
pub(crate) fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
