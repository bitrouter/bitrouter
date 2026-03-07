#[derive(Debug, Clone)]
pub enum Warning {
    Unsupported {
        feature: String,
        details: Option<String>,
    },
    Capability {
        feature: String,
        details: Option<String>,
    },
    Other {
        message: String,
    },
}
