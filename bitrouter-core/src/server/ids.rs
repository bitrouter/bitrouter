use std::fmt;

macro_rules! define_id {
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
                f.write_str(&self.0)
            }
        }
    };
}

define_id!(AccountId);
define_id!(ApiKeyId);
define_id!(SessionId);
define_id!(BlobId);
define_id!(RequestId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_creation_and_display() {
        let id = AccountId::new("acc_123");
        assert_eq!(id.as_str(), "acc_123");
        assert_eq!(id.to_string(), "acc_123");
    }

    #[test]
    fn id_equality() {
        let a = ApiKeyId::new("key_1");
        let b = ApiKeyId::new("key_1");
        let c = ApiKeyId::new("key_2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn id_from_string() {
        let id = SessionId::new(String::from("sess_abc"));
        assert_eq!(id.as_str(), "sess_abc");
    }

    #[test]
    fn id_clone() {
        let id = BlobId::new("blob_1");
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    #[test]
    fn id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(RequestId::new("req_1"));
        set.insert(RequestId::new("req_1"));
        assert_eq!(set.len(), 1);
    }
}
