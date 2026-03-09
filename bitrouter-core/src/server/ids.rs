use std::fmt::{Display, Formatter};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

string_id!(AccountId);
string_id!(ApiKeyId);
string_id!(SessionId);
string_id!(BlobId);
string_id!(RequestId);
