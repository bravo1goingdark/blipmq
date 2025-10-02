//! Authentication and Authorization system for BlipMQ
//!
//! Provides secure access control with:
//! - API key authentication
//! - JWT token-based authentication  
//! - Role-based access control (RBAC)
//! - Topic-level permissions
//! - Rate limiting per user/role
//! - Audit logging

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use base64::{Engine as _, engine::general_purpose};

use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

/// User permissions for topic operations
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    /// Can read messages from topics
    Subscribe,
    /// Can write messages to topics
    Publish,
    /// Can create new topics
    CreateTopic,
    /// Can delete topics
    DeleteTopic,
    /// Can manage topic configuration
    ManageTopic,
    /// Can view topic metrics and status
    ViewTopic,
    /// Administrative access
    Admin,
    /// Can manage users and permissions
    ManageUsers,
}

/// Role definition with associated permissions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub description: String,
    pub permissions: HashSet<Permission>,
    pub topic_patterns: Vec<String>, // Glob patterns for topic access
    pub rate_limit_per_second: Option<u64>,
    pub max_message_size: Option<usize>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Role {
    pub fn new(name: String, description: String, permissions: HashSet<Permission>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            name,
            description,
            permissions,
            topic_patterns: vec!["*".to_string()], // Default: access to all topics
            rate_limit_per_second: None,
            max_message_size: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn has_permission(&self, permission: &Permission) -> bool {
        self.permissions.contains(permission) || self.permissions.contains(&Permission::Admin)
    }

    pub fn can_access_topic(&self, topic: &str) -> bool {
        for pattern in &self.topic_patterns {
            if glob_match(pattern, topic) {
                return true;
            }
        }
        false
    }
}

/// User account with authentication credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: Option<String>, // For password-based auth
    pub api_keys: Vec<ApiKey>,
    pub roles: Vec<String>,
    pub enabled: bool,
    pub last_login: Option<u64>,
    pub login_count: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

impl User {
    pub fn new(id: String, username: String, email: Option<String>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            id,
            username,
            email,
            password_hash: None,
            api_keys: Vec::new(),
            roles: vec!["user".to_string()], // Default role
            enabled: true,
            last_login: None,
            login_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn set_password(&mut self, password: &str) {
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        // In production, use proper salt and key stretching (bcrypt, scrypt, etc.)
        self.password_hash = Some(format!("{:x}", hasher.finalize()));
    }

    pub fn verify_password(&self, password: &str) -> bool {
        if let Some(ref hash) = self.password_hash {
            let mut hasher = Sha256::new();
            hasher.update(password.as_bytes());
            let computed_hash = format!("{:x}", hasher.finalize());
            computed_hash == *hash
        } else {
            false
        }
    }

    pub fn add_api_key(&mut self, key: ApiKey) {
        self.api_keys.push(key);
        self.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
    }
}

/// API key for authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub key_hash: String,
    pub permissions: HashSet<Permission>,
    pub topic_patterns: Vec<String>,
    pub expires_at: Option<u64>,
    pub last_used: Option<u64>,
    pub usage_count: u64,
    pub enabled: bool,
    pub created_at: u64,
}

impl ApiKey {
    pub fn new(name: String, key: &str) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let key_hash = format!("{:x}", hasher.finalize());

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            key_hash,
            permissions: HashSet::new(),
            topic_patterns: vec!["*".to_string()],
            expires_at: None,
            last_used: None,
            usage_count: 0,
            enabled: true,
            created_at: now,
        }
    }

    pub fn verify_key(&self, key: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let computed_hash = format!("{:x}", hasher.finalize());
        computed_hash == self.key_hash
    }

    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            now >= expires_at
        } else {
            false
        }
    }

    pub fn record_usage(&mut self) {
        self.usage_count += 1;
        self.last_used = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        );
    }
}

/// JWT token claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,        // Subject (user ID)
    pub username: String,   // Username
    pub roles: Vec<String>, // User roles
    pub exp: u64,          // Expiration timestamp
    pub iat: u64,          // Issued at timestamp
    pub iss: String,       // Issuer
}

/// Authentication result
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: String,
    pub username: String,
    pub roles: Vec<String>,
    pub permissions: HashSet<Permission>,
    pub topic_patterns: Vec<String>,
    pub rate_limit_per_second: Option<u64>,
    pub max_message_size: Option<usize>,
    pub authenticated_at: u64,
}

impl AuthContext {
    pub fn has_permission(&self, permission: &Permission) -> bool {
        self.permissions.contains(permission) || self.permissions.contains(&Permission::Admin)
    }

    pub fn can_access_topic(&self, topic: &str) -> bool {
        for pattern in &self.topic_patterns {
            if glob_match(pattern, topic) {
                return true;
            }
        }
        false
    }

    pub fn can_publish_to_topic(&self, topic: &str) -> bool {
        self.has_permission(&Permission::Publish) && self.can_access_topic(topic)
    }

    pub fn can_subscribe_to_topic(&self, topic: &str) -> bool {
        self.has_permission(&Permission::Subscribe) && self.can_access_topic(topic)
    }
}

/// Rate limiting tracker
#[derive(Debug)]
pub struct RateLimiter {
    requests: DashMap<String, Vec<u64>>, // user_id -> timestamps
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            requests: DashMap::new(),
        }
    }

    pub fn check_rate_limit(&self, user_id: &str, limit_per_second: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut entry = self.requests.entry(user_id.to_string()).or_insert_with(Vec::new);
        
        // Remove old entries (older than 1 second)
        entry.retain(|&timestamp| now - timestamp < 1000);
        
        if entry.len() >= limit_per_second as usize {
            false // Rate limit exceeded
        } else {
            entry.push(now);
            true
        }
    }

    pub fn cleanup_old_entries(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Remove entries older than 5 minutes
        self.requests.retain(|_, timestamps| {
            timestamps.retain(|&timestamp| now - timestamp < 300_000);
            !timestamps.is_empty()
        });
    }
}

/// Main authentication and authorization manager
pub struct AuthManager {
    users: Arc<RwLock<HashMap<String, User>>>,
    roles: Arc<RwLock<HashMap<String, Role>>>,
    rate_limiter: RateLimiter,
    jwt_secret: String,
    jwt_expiry_seconds: u64,
}

impl AuthManager {
    pub fn new(jwt_secret: String) -> Self {
        let mut auth_manager = Self {
            users: Arc::new(RwLock::new(HashMap::new())),
            roles: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: RateLimiter::new(),
            jwt_secret,
            jwt_expiry_seconds: 24 * 60 * 60, // 24 hours
        };

        // Initialize default roles
        auth_manager.init_default_roles();
        auth_manager
    }

    fn init_default_roles(&mut self) {
        let mut roles = self.roles.write();

        // Admin role with all permissions
        let admin_permissions = vec![
            Permission::Subscribe,
            Permission::Publish,
            Permission::CreateTopic,
            Permission::DeleteTopic,
            Permission::ManageTopic,
            Permission::ViewTopic,
            Permission::Admin,
            Permission::ManageUsers,
        ].into_iter().collect();

        roles.insert(
            "admin".to_string(),
            Role::new(
                "admin".to_string(),
                "Full administrative access".to_string(),
                admin_permissions,
            ),
        );

        // Producer role - can publish
        let producer_permissions = vec![Permission::Publish, Permission::ViewTopic]
            .into_iter()
            .collect();

        roles.insert(
            "producer".to_string(),
            Role::new(
                "producer".to_string(),
                "Can publish messages to topics".to_string(),
                producer_permissions,
            ),
        );

        // Consumer role - can subscribe
        let consumer_permissions = vec![Permission::Subscribe, Permission::ViewTopic]
            .into_iter()
            .collect();

        roles.insert(
            "consumer".to_string(),
            Role::new(
                "consumer".to_string(),
                "Can subscribe to topics and receive messages".to_string(),
                consumer_permissions,
            ),
        );

        // User role - basic access
        let user_permissions = vec![Permission::Subscribe, Permission::Publish, Permission::ViewTopic]
            .into_iter()
            .collect();

        let mut user_role = Role::new(
            "user".to_string(),
            "Basic user with publish/subscribe access".to_string(),
            user_permissions,
        );
        user_role.rate_limit_per_second = Some(100); // 100 requests/second
        user_role.max_message_size = Some(1024 * 1024); // 1MB

        roles.insert("user".to_string(), user_role);
    }

    /// Authenticates using API key
    pub async fn authenticate_api_key(&self, api_key: &str) -> Option<AuthContext> {
        let users = self.users.read();
        
        for user in users.values() {
            if !user.enabled {
                continue;
            }

            for key in &user.api_keys {
                if key.enabled && !key.is_expired() && key.verify_key(api_key) {
                    return Some(self.build_auth_context(user).await);
                }
            }
        }

        None
    }

    /// Authenticates using username and password
    pub async fn authenticate_password(&self, username: &str, password: &str) -> Option<AuthContext> {
        let users = self.users.read();
        
        for user in users.values() {
            if user.enabled && user.username == username && user.verify_password(password) {
                return Some(self.build_auth_context(user).await);
            }
        }

        None
    }

    /// Generates JWT token for authenticated user
    pub fn generate_jwt(&self, user: &User) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = JwtClaims {
            sub: user.id.clone(),
            username: user.username.clone(),
            roles: user.roles.clone(),
            exp: now + self.jwt_expiry_seconds,
            iat: now,
            iss: "blipmq".to_string(),
        };

        // In production, use proper JWT library like jsonwebtoken
        let token_data = serde_json::to_string(&claims)?;
        let signature = self.sign_jwt(&token_data)?;
        
        Ok(format!("{}.{}", general_purpose::STANDARD.encode(&token_data), signature))
    }

    /// Validates JWT token
    pub async fn validate_jwt(&self, token: &str) -> Option<AuthContext> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 2 {
            return None;
        }

        let token_data = parts[0];
        let signature = parts[1];

        // Decode claims first to get the raw JSON data for signature verification
        let decoded = general_purpose::STANDARD.decode(token_data).ok()?;
        let raw_json = std::str::from_utf8(&decoded).ok()?;
        
        // Verify signature using raw JSON data (same as during generation)
        let expected_signature = self.sign_jwt(raw_json).ok()?;
        if expected_signature != signature {
            return None;
        }
        
        let claims: JwtClaims = serde_json::from_slice(&decoded).ok()?;

        // Check expiration
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if claims.exp <= now {
            return None; // Token expired
        }

        // Build auth context from claims
        let users = self.users.read();
        if let Some(user) = users.get(&claims.sub) {
            if user.enabled {
                return Some(self.build_auth_context(user).await);
            }
        }

        None
    }

    fn sign_jwt(&self, data: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        hasher.update(self.jwt_secret.as_bytes());
        Ok(general_purpose::STANDARD.encode(hasher.finalize()))
    }

    /// Builds authentication context for a user
    async fn build_auth_context(&self, user: &User) -> AuthContext {
        let mut all_permissions = HashSet::new();
        let mut all_patterns = Vec::new();
        let mut rate_limit = None;
        let mut max_message_size = None;

        let roles = self.roles.read();
        for role_name in &user.roles {
            if let Some(role) = roles.get(role_name) {
                all_permissions.extend(role.permissions.iter().cloned());
                all_patterns.extend(role.topic_patterns.iter().cloned());
                
                if rate_limit.is_none() {
                    rate_limit = role.rate_limit_per_second;
                }
                
                if max_message_size.is_none() {
                    max_message_size = role.max_message_size;
                }
            }
        }

        // Remove duplicates from topic patterns
        all_patterns.sort();
        all_patterns.dedup();

        AuthContext {
            user_id: user.id.clone(),
            username: user.username.clone(),
            roles: user.roles.clone(),
            permissions: all_permissions,
            topic_patterns: all_patterns,
            rate_limit_per_second: rate_limit,
            max_message_size,
            authenticated_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }

    /// Checks if a user can perform an operation within rate limits
    pub fn check_rate_limit(&self, auth_context: &AuthContext) -> bool {
        if let Some(limit) = auth_context.rate_limit_per_second {
            self.rate_limiter.check_rate_limit(&auth_context.user_id, limit)
        } else {
            true // No rate limit
        }
    }

    /// User management functions
    pub fn create_user(&self, mut user: User) -> Result<(), String> {
        let mut users = self.users.write();
        
        if users.contains_key(&user.id) {
            return Err("User already exists".to_string());
        }

        // Ensure user has at least the default role
        if user.roles.is_empty() {
            user.roles.push("user".to_string());
        }

        let user_id = user.id.clone();
        let username = user.username.clone();
        users.insert(user_id.clone(), user);
        info!("Created new user: {}", username);
        Ok(())
    }

    pub fn update_user(&self, user_id: &str, updated_user: User) -> Result<(), String> {
        let mut users = self.users.write();
        
        if let Some(user) = users.get_mut(user_id) {
            *user = updated_user;
            info!("Updated user: {}", user.username);
            Ok(())
        } else {
            Err("User not found".to_string())
        }
    }

    pub fn delete_user(&self, user_id: &str) -> Result<(), String> {
        let mut users = self.users.write();
        
        if let Some(user) = users.remove(user_id) {
            info!("Deleted user: {}", user.username);
            Ok(())
        } else {
            Err("User not found".to_string())
        }
    }

    pub fn get_user(&self, user_id: &str) -> Option<User> {
        self.users.read().get(user_id).cloned()
    }

    pub fn list_users(&self) -> Vec<User> {
        self.users.read().values().cloned().collect()
    }

    /// Role management functions
    pub fn create_role(&self, role: Role) -> Result<(), String> {
        let mut roles = self.roles.write();
        
        if roles.contains_key(&role.name) {
            return Err("Role already exists".to_string());
        }

        roles.insert(role.name.clone(), role);
        Ok(())
    }

    pub fn get_role(&self, role_name: &str) -> Option<Role> {
        self.roles.read().get(role_name).cloned()
    }

    pub fn list_roles(&self) -> Vec<Role> {
        self.roles.read().values().cloned().collect()
    }

    /// Cleanup old rate limit entries
    pub fn cleanup(&self) {
        self.rate_limiter.cleanup_old_entries();
    }
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new("default-jwt-secret-change-in-production".to_string())
    }
}

/// Simple glob pattern matching
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    
    // Simple implementation - in production use proper glob library
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            let prefix = parts[0];
            let suffix = parts[1];
            return text.starts_with(prefix) && text.ends_with(suffix);
        }
    }
    
    pattern == text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_authentication() {
        let auth_manager = AuthManager::new("test-secret".to_string());
        
        // Create a test user
        let mut user = User::new(
            "user1".to_string(),
            "testuser".to_string(),
            Some("test@example.com".to_string()),
        );
        user.set_password("password123");
        
        // Create API key
        let api_key_value = "test-api-key-12345";
        let api_key = ApiKey::new("Test Key".to_string(), api_key_value);
        user.add_api_key(api_key);
        
        auth_manager.create_user(user).unwrap();
        
        // Test password authentication
        let auth_context = auth_manager
            .authenticate_password("testuser", "password123")
            .await
            .unwrap();
        
        assert_eq!(auth_context.username, "testuser");
        assert!(auth_context.has_permission(&Permission::Subscribe));
        assert!(auth_context.has_permission(&Permission::Publish));
        
        // Test API key authentication
        let auth_context2 = auth_manager
            .authenticate_api_key(api_key_value)
            .await
            .unwrap();
        
        assert_eq!(auth_context2.user_id, auth_context.user_id);
    }

    #[tokio::test]
    async fn test_jwt_tokens() {
        let auth_manager = AuthManager::new("test-secret".to_string());
        
        let user = User::new(
            "user1".to_string(),
            "testuser".to_string(),
            Some("test@example.com".to_string()),
        );
        
        auth_manager.create_user(user.clone()).unwrap();
        
        // Generate JWT
        let token = auth_manager.generate_jwt(&user).unwrap();
        assert!(!token.is_empty());
        
        // Validate JWT
        let auth_context = auth_manager.validate_jwt(&token).await.unwrap();
        assert_eq!(auth_context.username, "testuser");
    }

    #[test]
    fn test_role_permissions() {
        let mut permissions = HashSet::new();
        permissions.insert(Permission::Subscribe);
        permissions.insert(Permission::Publish);
        
        let role = Role::new(
            "test_role".to_string(),
            "Test role".to_string(),
            permissions,
        );
        
        assert!(role.has_permission(&Permission::Subscribe));
        assert!(role.has_permission(&Permission::Publish));
        assert!(!role.has_permission(&Permission::Admin));
        
        // Test topic access
        assert!(role.can_access_topic("any-topic")); // Default pattern "*"
    }

    #[test]
    fn test_rate_limiting() {
        let rate_limiter = RateLimiter::new();
        
        // Should allow up to limit
        for _ in 0..5 {
            assert!(rate_limiter.check_rate_limit("user1", 10));
        }
        
        // Check we're still within limit
        assert!(rate_limiter.check_rate_limit("user1", 10));
    }

    #[test]
    fn test_glob_matching() {
        assert!(glob_match("*", "any-topic"));
        assert!(glob_match("user.*", "user.events"));
        assert!(glob_match("*.logs", "app.logs"));
        assert!(!glob_match("user.*", "admin.events"));
    }
}