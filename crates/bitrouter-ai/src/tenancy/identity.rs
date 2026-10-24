//! Distinct, bounded identities and primitive wire values.

use serde::{Deserialize, Serialize};

use super::validation::{ValidationError, require};

macro_rules! identifier {
    ($($name:ident: $doc:literal),+ $(,)?) => {$(
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            /// Borrow the validated ASCII identifier.
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl TryFrom<String> for $name {
            type Error = ValidationError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                require(value.len() <= 128
                    && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                    && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c)),
                    "identifier syntax")?;
                Ok(Self(value))
            }
        }
        impl TryFrom<&str> for $name {
            type Error = ValidationError;
            fn try_from(value: &str) -> Result<Self, Self::Error> { Self::try_from(value.to_owned()) }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0 } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
        }
    )+};
}

identifier! {
    InstallationId: "Persistent identity of an authority store, never reused after retirement.",
    InstanceId: "Identity of one running process, refreshed at startup.",
    TenantId: "Resource ownership domain, independent of directory users.",
    PrincipalId: "Stable authenticated actor identity, independent of credentials.",
    CredentialId: "Nonsecret identity of an ingress or upstream credential.",
    BindingId: "Identity of one principal-to-tenant grant.",
    OwnerId: "Identity qualified by an explicit tenant or operator-pool owner kind.",
    ResourceId: "Resource identity qualified by owner and kind, never by content hash alone.",
    RevisionId: "Immutable revision identity; not an implicit current-version selector.",
    OperationId: "Management command identity scoped by installation and authenticated principal.",
    ExecutionId: "Identity of one actual invocation, never an idempotency key.",
    AttemptId: "Identity of one provider attempt within an invocation.",
    CorrelationId: "Query correlation; repeated values never deduplicate execution.",
    ProviderId: "Identity of the provider actually selected for an attempt.",
    AccountId: "Nonsecret identity of the account actually selected for an attempt.",
}

/// Monotonic local authority revision, exactly representable in JSON clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(into = "u64")]
pub struct AuthorityRevision(u64);

impl AuthorityRevision {
    /// The largest integer representable exactly by every supported JSON client.
    pub const MAX: u64 = 9_007_199_254_740_991;
    /// Get the numeric revision.
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl TryFrom<u64> for AuthorityRevision {
    type Error = ValidationError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        require(value <= Self::MAX, "authority revision range")?;
        Ok(Self(value))
    }
}
impl<'de> Deserialize<'de> for AuthorityRevision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value: u64 = super::number::integer(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}
impl From<AuthorityRevision> for u64 {
    fn from(value: AuthorityRevision) -> Self {
        value.0
    }
}

/// SHA-256 digest encoded as 64 lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);
impl Digest {
    /// Borrow the canonical hexadecimal digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Digest {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        require(
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "digest syntax",
        )?;
        Ok(Self(value))
    }
}
impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.0
    }
}

/// RFC 3339 timestamp with an explicit uppercase UTC `Z` suffix.
///
/// This value validates syntax and calendar bounds, without a clock or I/O.
/// Authority expiry comparisons belong to the service, not this wire type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);
impl Timestamp {
    /// Borrow the validated UTC timestamp.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Timestamp {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        require(valid_timestamp(&value), "UTC timestamp syntax")?;
        Ok(Self(value))
    }
}
impl TryFrom<&str> for Timestamp {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_owned())
    }
}
impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

fn valid_timestamp(value: &str) -> bool {
    // UTC profile of RFC 3339 §§5.6–5.7; no timezone database or runtime is needed.
    // https://www.rfc-editor.org/rfc/rfc3339#section-5.6
    let b = value.as_bytes();
    if b.len() < 20
        || b.last() != Some(&b'Z')
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return false;
    }
    let number = |start: usize, end: usize| -> Option<u32> {
        b[start..end].iter().try_fold(0, |acc, &c| {
            c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
        })
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        number(0, 4),
        number(5, 7),
        number(8, 10),
        number(11, 13),
        number(14, 16),
        number(17, 19),
    ) else {
        return false;
    };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => return false,
    };
    let leap_second = second == 60 && hour == 23 && minute == 59 && day == days;
    (1..=days).contains(&day)
        && hour <= 23
        && minute <= 59
        && (second <= 59 || leap_second)
        && (b.len() == 20
            || (b.len() > 21 && b[19] == b'.' && b[20..b.len() - 1].iter().all(u8::is_ascii_digit)))
}
