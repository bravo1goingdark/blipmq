use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new<S: Into<String>>(value: S) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Trait used by the network layer to validate API keys.
pub trait ApiKeyValidator: Send + Sync {
    fn validate(&self, key: &ApiKey) -> bool;
}

#[derive(Debug, Clone)]
pub struct StaticApiKeyValidator {
    valid_keys: HashSet<ApiKey>,
}

impl StaticApiKeyValidator {
    /// Create a new validator from an iterator of string keys.
    pub fn from_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let valid_keys = keys.into_iter().map(ApiKey::new).collect();
        Self { valid_keys }
    }
}

impl ApiKeyValidator for StaticApiKeyValidator {
    fn validate(&self, key: &ApiKey) -> bool {
        self.valid_keys.contains(key)
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    valid_keys: HashSet<ApiKey>,
}

impl AuthConfig {
    pub fn from_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let valid_keys = keys.into_iter().map(ApiKey::new).collect();
        Self { valid_keys }
    }

    pub fn is_valid(&self, key: &ApiKey) -> bool {
        self.valid_keys.contains(key)
    }
}
