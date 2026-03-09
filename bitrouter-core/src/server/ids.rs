use std::{fmt, str::FromStr};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = &'static str;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.is_empty() {
                    Err("id cannot be empty")
                } else {
                    Ok(Self::new(s))
                }
            }
        }
    };
}

id_type!(AccountId);
id_type!(ApiKeyId);
id_type!(SessionId);
id_type!(BlobId);
id_type!(RequestId);

#[cfg(test)]
mod tests {
    use super::{AccountId, RequestId};

    #[test]
    fn ids_round_trip_as_str_and_display() {
        let id = AccountId::new("acct_123");
        assert_eq!(id.as_str(), "acct_123");
        assert_eq!(id.to_string(), "acct_123");
    }

    #[test]
    fn ids_reject_empty_values() {
        assert!("".parse::<RequestId>().is_err());
    }
}
