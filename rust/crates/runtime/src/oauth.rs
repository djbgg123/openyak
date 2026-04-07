use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::config::OAuthConfig;

const OAUTH_KEYRING_SERVICE: &str = "openyak";
// Preserve legacy migration support for credentials stored by earlier claw builds.
const LEGACY_OAUTH_KEYRING_SERVICE: &str = "claw-code";
const OAUTH_KEYRING_USER_PREFIX: &str = "oauth:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodePair {
    pub verifier: String,
    pub challenge: String,
    pub challenge_method: PkceChallengeMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkceChallengeMethod {
    S256,
}

impl PkceChallengeMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S256 => "S256",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationRequest {
    pub authorize_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: String,
    pub code_challenge: String,
    pub code_challenge_method: PkceChallengeMethod,
    pub extra_params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthTokenExchangeRequest {
    pub grant_type: &'static str,
    pub code: String,
    pub redirect_uri: String,
    pub client_id: String,
    pub code_verifier: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthRefreshRequest {
    pub grant_type: &'static str,
    pub refresh_token: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredOAuthCredentials {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Vec<String>,
}

impl From<OAuthTokenSet> for StoredOAuthCredentials {
    fn from(value: OAuthTokenSet) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_at: value.expires_at,
            scopes: value.scopes,
        }
    }
}

impl From<StoredOAuthCredentials> for OAuthTokenSet {
    fn from(value: StoredOAuthCredentials) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_at: value.expires_at,
            scopes: value.scopes,
        }
    }
}

impl OAuthAuthorizationRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        redirect_uri: impl Into<String>,
        state: impl Into<String>,
        pkce: &PkceCodePair,
    ) -> Self {
        Self {
            authorize_url: config.authorize_url.clone(),
            client_id: config.client_id.clone(),
            redirect_uri: redirect_uri.into(),
            scopes: config.scopes.clone(),
            state: state.into(),
            code_challenge: pkce.challenge.clone(),
            code_challenge_method: pkce.challenge_method,
            extra_params: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_extra_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_params.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn build_url(&self) -> String {
        let mut params = vec![
            ("response_type", "code".to_string()),
            ("client_id", self.client_id.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("scope", self.scopes.join(" ")),
            ("state", self.state.clone()),
            ("code_challenge", self.code_challenge.clone()),
            (
                "code_challenge_method",
                self.code_challenge_method.as_str().to_string(),
            ),
        ];
        params.extend(
            self.extra_params
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone())),
        );
        let query = params
            .into_iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(&value)))
            .collect::<Vec<_>>()
            .join("&");
        format!(
            "{}{}{}",
            self.authorize_url,
            if self.authorize_url.contains('?') {
                '&'
            } else {
                '?'
            },
            query
        )
    }
}

impl OAuthTokenExchangeRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        code: impl Into<String>,
        state: impl Into<String>,
        verifier: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            grant_type: "authorization_code",
            code: code.into(),
            redirect_uri: redirect_uri.into(),
            client_id: config.client_id.clone(),
            code_verifier: verifier.into(),
            state: state.into(),
        }
    }

    #[must_use]
    pub fn form_params(&self) -> BTreeMap<&str, String> {
        BTreeMap::from([
            ("grant_type", self.grant_type.to_string()),
            ("code", self.code.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", self.code_verifier.clone()),
            ("state", self.state.clone()),
        ])
    }
}

impl OAuthRefreshRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        refresh_token: impl Into<String>,
        scopes: Option<Vec<String>>,
    ) -> Self {
        Self {
            grant_type: "refresh_token",
            refresh_token: refresh_token.into(),
            client_id: config.client_id.clone(),
            scopes: scopes.unwrap_or_else(|| config.scopes.clone()),
        }
    }

    #[must_use]
    pub fn form_params(&self) -> BTreeMap<&str, String> {
        BTreeMap::from([
            ("grant_type", self.grant_type.to_string()),
            ("refresh_token", self.refresh_token.clone()),
            ("client_id", self.client_id.clone()),
            ("scope", self.scopes.join(" ")),
        ])
    }
}

pub fn generate_pkce_pair() -> io::Result<PkceCodePair> {
    let verifier = generate_random_token(32)?;
    Ok(PkceCodePair {
        challenge: code_challenge_s256(&verifier),
        verifier,
        challenge_method: PkceChallengeMethod::S256,
    })
}

pub fn generate_state() -> io::Result<String> {
    generate_random_token(32)
}

#[must_use]
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_encode(&digest)
}

#[must_use]
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

pub fn credentials_path() -> io::Result<PathBuf> {
    Ok(credentials_home_dir().join("credentials.json"))
}

pub fn load_oauth_credentials() -> io::Result<Option<OAuthTokenSet>> {
    let current_path = credentials_path()?;
    if let Some(token_set) = load_oauth_credentials_from_path(&current_path)? {
        return Ok(Some(token_set));
    }

    let legacy_path = legacy_credentials_path();
    if legacy_path == current_path {
        return Ok(None);
    }

    let legacy_token_set = load_oauth_credentials_from_path(&legacy_path)?;
    if let Some(token_set) = legacy_token_set {
        persist_oauth_credentials_at_path(&current_path, &token_set)?;
        let _ = clear_oauth_credentials_at_path(&legacy_path);
        return Ok(Some(token_set));
    }

    Ok(None)
}

pub fn save_oauth_credentials(token_set: &OAuthTokenSet) -> io::Result<()> {
    let current_path = credentials_path()?;
    persist_oauth_credentials_at_path(&current_path, token_set)?;

    let legacy_path = legacy_credentials_path();
    if legacy_path != current_path {
        let _ = clear_oauth_credentials_at_path(&legacy_path);
    }
    Ok(())
}

pub fn clear_oauth_credentials() -> io::Result<()> {
    let current_path = credentials_path()?;
    clear_oauth_credentials_at_path(&current_path)?;

    let legacy_path = legacy_credentials_path();
    if legacy_path != current_path {
        let _ = clear_oauth_credentials_at_path(&legacy_path);
    }

    Ok(())
}

pub fn parse_oauth_callback_request_target(target: &str) -> Result<OAuthCallbackParams, String> {
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    if path != "/callback" {
        return Err(format!("unexpected callback path: {path}"));
    }
    parse_oauth_callback_query(query)
}

pub fn parse_oauth_callback_input(input: &str) -> Result<OAuthCallbackParams, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("missing OAuth callback input".to_string());
    }

    if let Some((_, query)) = trimmed.split_once('?') {
        return parse_oauth_callback_query(query);
    }
    if trimmed.contains('=') {
        return parse_oauth_callback_query(trimmed);
    }

    Err("OAuth callback input must include a query string".to_string())
}

pub fn parse_oauth_callback_query(query: &str) -> Result<OAuthCallbackParams, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .map_or((pair, ""), |(key, value)| (key, value));
        params.insert(percent_decode(key)?, percent_decode(value)?);
    }
    Ok(OAuthCallbackParams {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
    })
}

fn generate_random_token(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(base64url_encode(&buffer))
}

fn credentials_home_dir() -> PathBuf {
    crate::paths::default_openyak_home()
}

fn legacy_credentials_path() -> PathBuf {
    crate::paths::legacy_claw_home().join("credentials.json")
}

fn load_oauth_credentials_from_file(path: &Path) -> io::Result<Option<OAuthTokenSet>> {
    let root = read_credentials_root(path)?;
    parse_oauth_credentials_from_root(&root)
}

fn load_oauth_credentials_from_secret(secret: &[u8]) -> io::Result<OAuthTokenSet> {
    let stored = serde_json::from_slice::<StoredOAuthCredentials>(secret)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(stored.into())
}

fn encode_oauth_credentials(token_set: &OAuthTokenSet) -> io::Result<Vec<u8>> {
    serde_json::to_vec(&StoredOAuthCredentials::from(token_set.clone()))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn load_oauth_credentials_from_path(path: &Path) -> io::Result<Option<OAuthTokenSet>> {
    if let Some(secret) = load_secret_from_system_store(path)? {
        return load_oauth_credentials_from_secret(&secret).map(Some);
    }

    let token_set = load_oauth_credentials_from_file(path)?;
    if let Some(token_set) = &token_set {
        let encoded = encode_oauth_credentials(token_set)?;
        if save_secret_to_system_store(path, &encoded)? {
            remove_oauth_credentials_from_file(path)?;
        }
    }
    Ok(token_set)
}

fn persist_oauth_credentials_at_path(path: &Path, token_set: &OAuthTokenSet) -> io::Result<()> {
    let encoded = encode_oauth_credentials(token_set)?;
    if save_secret_to_system_store(path, &encoded)? {
        remove_oauth_credentials_from_file(path)?;
        return Ok(());
    }
    save_oauth_credentials_to_file(path, token_set)
}

fn save_oauth_credentials_to_file(path: &Path, token_set: &OAuthTokenSet) -> io::Result<()> {
    let mut root = read_credentials_root(path)?;
    root.insert(
        "oauth".to_string(),
        serde_json::to_value(StoredOAuthCredentials::from(token_set.clone()))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    );
    write_credentials_root(path, &root)
}

fn remove_oauth_credentials_from_file(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_credentials_root(path)?;
    root.remove("oauth");
    if root.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        write_credentials_root(path, &root)
    }
}

fn clear_oauth_credentials_at_path(path: &Path) -> io::Result<()> {
    remove_oauth_credentials_from_file(path)?;
    clear_secret_from_system_store(path)
}

fn parse_oauth_credentials_from_root(
    root: &Map<String, Value>,
) -> io::Result<Option<OAuthTokenSet>> {
    let Some(oauth) = root.get("oauth") else {
        return Ok(None);
    };
    if oauth.is_null() {
        return Ok(None);
    }
    let stored = serde_json::from_value::<StoredOAuthCredentials>(oauth.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(stored.into()))
}

fn read_credentials_root(path: &Path) -> io::Result<Map<String, Value>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                .as_object()
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "credentials file must contain a JSON object",
                    )
                })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error),
    }
}

fn write_credentials_root(path: &Path, root: &Map<String, Value>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        restrict_credentials_dir_permissions(parent)?;
    }
    let rendered = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp_path = path.with_extension("json.tmp");
    write_restricted_credentials_file(&temp_path, format!("{rendered}\n").as_bytes())?;
    replace_credentials_file(&temp_path, path)?;
    restrict_credentials_file_permissions(path)
}

fn write_restricted_credentials_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    restrict_credentials_file_permissions(path)
}

fn replace_credentials_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    if path.exists() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::rename(temp_path, path)
}

#[cfg(unix)]
fn restrict_credentials_dir_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn restrict_credentials_dir_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_credentials_file_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn restrict_credentials_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn system_store_user(path: &Path) -> String {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let mut user = String::from(OAUTH_KEYRING_USER_PREFIX);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut user, "{byte:02x}");
    }
    user
}

#[cfg(test)]
fn system_store_key(path: &Path) -> String {
    format!("{OAUTH_KEYRING_SERVICE}:{}", system_store_user(path))
}

#[cfg(test)]
fn legacy_system_store_key(path: &Path) -> String {
    format!("{LEGACY_OAUTH_KEYRING_SERVICE}:{}", system_store_user(path))
}

#[cfg(not(test))]
fn load_secret_from_system_store(path: &Path) -> io::Result<Option<Vec<u8>>> {
    if !system_store_supported() {
        return Ok(None);
    }

    let user = system_store_user(path);
    for service in [OAUTH_KEYRING_SERVICE, LEGACY_OAUTH_KEYRING_SERVICE] {
        let entry = match keyring::Entry::new(service, &user) {
            Ok(entry) => entry,
            Err(error) if keyring_error_is_fallbackable(&error) => return Ok(None),
            Err(error) => return Err(keyring_error_to_io(&error)),
        };

        match entry.get_secret() {
            Ok(secret) => return Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => {}
            Err(error) if keyring_error_is_fallbackable(&error) => return Ok(None),
            Err(error) => return Err(keyring_error_to_io(&error)),
        }
    }

    Ok(None)
}

#[cfg(not(test))]
fn save_secret_to_system_store(path: &Path, secret: &[u8]) -> io::Result<bool> {
    if !system_store_supported() {
        return Ok(false);
    }

    let entry = match keyring::Entry::new(OAUTH_KEYRING_SERVICE, &system_store_user(path)) {
        Ok(entry) => entry,
        Err(error) if keyring_error_is_fallbackable(&error) => return Ok(false),
        Err(error) => return Err(keyring_error_to_io(&error)),
    };

    match entry.set_secret(secret) {
        Ok(()) => Ok(true),
        Err(error) if keyring_error_is_fallbackable(&error) => Ok(false),
        Err(error) => Err(keyring_error_to_io(&error)),
    }
}

#[cfg(not(test))]
fn clear_secret_from_system_store(path: &Path) -> io::Result<()> {
    if !system_store_supported() {
        return Ok(());
    }

    let user = system_store_user(path);
    for service in [OAUTH_KEYRING_SERVICE, LEGACY_OAUTH_KEYRING_SERVICE] {
        let entry = match keyring::Entry::new(service, &user) {
            Ok(entry) => entry,
            Err(error) if keyring_error_is_fallbackable(&error) => return Ok(()),
            Err(error) => return Err(keyring_error_to_io(&error)),
        };

        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) if keyring_error_is_fallbackable(&error) => return Ok(()),
            Err(error) => return Err(keyring_error_to_io(&error)),
        }
    }

    Ok(())
}

#[cfg(not(test))]
const fn system_store_supported() -> bool {
    cfg!(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd"
    ))
}

#[cfg(not(test))]
fn keyring_error_is_fallbackable(error: &keyring::Error) -> bool {
    matches!(
        error,
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_)
    )
}

#[cfg(not(test))]
fn keyring_error_to_io(error: &keyring::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn load_secret_from_system_store(path: &Path) -> io::Result<Option<Vec<u8>>> {
    if !*test_system_store_enabled()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Ok(None);
    }
    let store = test_system_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(store
        .get(&system_store_key(path))
        .cloned()
        .or_else(|| store.get(&legacy_system_store_key(path)).cloned()))
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn save_secret_to_system_store(path: &Path, secret: &[u8]) -> io::Result<bool> {
    if !*test_system_store_enabled()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Ok(false);
    }
    let mut store = test_system_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store.insert(system_store_key(path), secret.to_vec());
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn clear_secret_from_system_store(path: &Path) -> io::Result<()> {
    if !*test_system_store_enabled()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Ok(());
    }
    let mut store = test_system_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store.remove(&system_store_key(path));
    store.remove(&legacy_system_store_key(path));
    Ok(())
}

#[cfg(test)]
fn clear_test_system_store() {
    let mut store = test_system_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store.clear();
    drop(store);
    *test_system_store_enabled()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
}

#[cfg(test)]
fn test_system_store() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

#[cfg(test)]
fn test_system_store_enabled() -> &'static std::sync::Mutex<bool> {
    static ENABLED: std::sync::OnceLock<std::sync::Mutex<bool>> = std::sync::OnceLock::new();
    ENABLED.get_or_init(|| std::sync::Mutex::new(true))
}

#[cfg(test)]
fn set_test_system_store_enabled(enabled: bool) {
    *test_system_store_enabled()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = enabled;
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let block = (u32::from(bytes[index]) << 16)
            | (u32::from(bytes[index + 1]) << 8)
            | u32::from(bytes[index + 2]);
        output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        output.push(TABLE[(block & 0x3F) as usize] as char);
        index += 3;
    }
    match bytes.len().saturating_sub(index) {
        1 => {
            let block = u32::from(bytes[index]) << 16;
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let block = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        }
        _ => {}
    }
    output
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Result<String, String> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = decode_hex(bytes[index + 1])?;
                let lo = decode_hex(bytes[index + 2])?;
                decoded.push((hi << 4) | lo);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|error| error.to_string())
}

fn decode_hex(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid percent-encoding byte: {byte}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        clear_oauth_credentials, clear_test_system_store, code_challenge_s256, credentials_path,
        generate_pkce_pair, generate_state, legacy_credentials_path, legacy_system_store_key,
        load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_input,
        parse_oauth_callback_query, parse_oauth_callback_request_target, save_oauth_credentials,
        set_test_system_store_enabled, system_store_key, OAuthAuthorizationRequest, OAuthConfig,
        OAuthRefreshRequest, OAuthTokenExchangeRequest, OAuthTokenSet,
    };

    fn sample_config() -> OAuthConfig {
        OAuthConfig {
            client_id: "runtime-client".to_string(),
            authorize_url: "https://console.test/oauth/authorize".to_string(),
            token_url: "https://console.test/oauth/token".to_string(),
            callback_port: Some(4545),
            manual_redirect_url: Some("https://console.test/oauth/callback".to_string()),
            scopes: vec!["org:read".to_string(), "user:write".to_string()],
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    fn temp_config_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-oauth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn cleanup_dir_if_exists(path: &Path) {
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cleanup temp dir: {error}"),
        }
    }

    #[test]
    fn s256_challenge_matches_expected_vector() {
        assert_eq!(
            code_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generates_pkce_pair_and_state() {
        let pair = generate_pkce_pair().expect("pkce pair");
        let state = generate_state().expect("state");
        assert!(!pair.verifier.is_empty());
        assert!(!pair.challenge.is_empty());
        assert!(!state.is_empty());
    }

    #[test]
    fn builds_authorize_url_and_form_requests() {
        let config = sample_config();
        let pair = generate_pkce_pair().expect("pkce");
        let url = OAuthAuthorizationRequest::from_config(
            &config,
            loopback_redirect_uri(4545),
            "state-123",
            &pair,
        )
        .with_extra_param("login_hint", "user@example.com")
        .build_url();
        assert!(url.starts_with("https://console.test/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=runtime-client"));
        assert!(url.contains("scope=org%3Aread%20user%3Awrite"));
        assert!(url.contains("login_hint=user%40example.com"));

        let exchange = OAuthTokenExchangeRequest::from_config(
            &config,
            "auth-code",
            "state-123",
            pair.verifier,
            loopback_redirect_uri(4545),
        );
        assert_eq!(
            exchange.form_params().get("grant_type").map(String::as_str),
            Some("authorization_code")
        );

        let refresh = OAuthRefreshRequest::from_config(&config, "refresh-token", None);
        assert_eq!(
            refresh.form_params().get("scope").map(String::as_str),
            Some("org:read user:write")
        );
    }

    #[test]
    fn oauth_credentials_round_trip_and_clear_preserves_other_fields() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        clear_test_system_store();
        std::env::set_var("OPENYAK_CONFIG_HOME", &config_home);
        let path = credentials_path().expect("credentials path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "{\"other\":\"value\"}\n").expect("seed credentials");

        let token_set = OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(123),
            scopes: vec!["scope:a".to_string()],
        };
        save_oauth_credentials(&token_set).expect("save credentials");
        assert_eq!(
            load_oauth_credentials().expect("load credentials"),
            Some(token_set)
        );
        let saved = std::fs::read_to_string(&path).expect("read saved file");
        assert!(saved.contains("\"other\": \"value\""));
        assert!(!saved.contains("\"oauth\""));
        assert_private_file_permissions(&path);

        clear_oauth_credentials().expect("clear credentials");
        assert_eq!(load_oauth_credentials().expect("load cleared"), None);
        let cleared = std::fs::read_to_string(&path).expect("read cleared file");
        assert!(cleared.contains("\"other\": \"value\""));
        assert!(!cleared.contains("\"oauth\""));
        assert_private_file_permissions(&path);

        std::env::remove_var("OPENYAK_CONFIG_HOME");
        clear_test_system_store();
        cleanup_dir_if_exists(&config_home);
    }

    #[test]
    fn loading_file_backed_credentials_keeps_file_when_system_store_is_unavailable() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        clear_test_system_store();
        set_test_system_store_enabled(false);
        std::env::set_var("OPENYAK_CONFIG_HOME", &config_home);

        let token_set = OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(123),
            scopes: vec!["scope:a".to_string()],
        };
        let path = credentials_path().expect("credentials path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(
            &path,
            serde_json::json!({
                "oauth": {
                    "accessToken": token_set.access_token,
                    "refreshToken": token_set.refresh_token,
                    "expiresAt": token_set.expires_at,
                    "scopes": token_set.scopes,
                }
            })
            .to_string(),
        )
        .expect("seed oauth file");

        assert_eq!(
            load_oauth_credentials().expect("load credentials"),
            Some(OAuthTokenSet {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_at: Some(123),
                scopes: vec!["scope:a".to_string()],
            })
        );
        let saved = std::fs::read_to_string(&path).expect("read saved file");
        assert!(saved.contains("\"oauth\""));

        std::env::remove_var("OPENYAK_CONFIG_HOME");
        set_test_system_store_enabled(true);
        clear_test_system_store();
        cleanup_dir_if_exists(&config_home);
    }

    #[test]
    fn loads_legacy_keyring_credentials_and_migrates_them_to_openyak_store() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        clear_test_system_store();
        std::env::set_var("OPENYAK_CONFIG_HOME", &config_home);

        let current_path = credentials_path().expect("current credentials path");
        let legacy_path = legacy_credentials_path();
        let token_set = OAuthTokenSet {
            access_token: "legacy-access".to_string(),
            refresh_token: Some("legacy-refresh".to_string()),
            expires_at: Some(456),
            scopes: vec!["scope:legacy".to_string()],
        };
        let encoded = serde_json::to_vec(&serde_json::json!({
            "accessToken": token_set.access_token,
            "refreshToken": token_set.refresh_token,
            "expiresAt": token_set.expires_at,
            "scopes": token_set.scopes,
        }))
        .expect("encode legacy credentials");

        {
            let mut store = super::test_system_store()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store.insert(legacy_system_store_key(&legacy_path), encoded);
        }

        assert_eq!(
            load_oauth_credentials().expect("load legacy credentials"),
            Some(OAuthTokenSet {
                access_token: "legacy-access".to_string(),
                refresh_token: Some("legacy-refresh".to_string()),
                expires_at: Some(456),
                scopes: vec!["scope:legacy".to_string()],
            })
        );

        let store = super::test_system_store()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(store.contains_key(&system_store_key(&current_path)));
        assert!(!store.contains_key(&legacy_system_store_key(&legacy_path)));
        drop(store);

        std::env::remove_var("OPENYAK_CONFIG_HOME");
        clear_test_system_store();
        cleanup_dir_if_exists(&config_home);
    }

    #[test]
    fn loads_legacy_file_credentials_and_migrates_them_to_openyak_store() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        clear_test_system_store();
        std::env::set_var("OPENYAK_CONFIG_HOME", &config_home);

        let current_path = credentials_path().expect("current credentials path");
        let legacy_path = legacy_credentials_path();
        std::fs::create_dir_all(legacy_path.parent().expect("legacy parent"))
            .expect("legacy parent");
        std::fs::write(
            &legacy_path,
            serde_json::json!({
                "oauth": {
                    "accessToken": "legacy-access",
                    "refreshToken": "legacy-refresh",
                    "expiresAt": 456,
                    "scopes": ["scope:legacy"],
                },
                "other": "value",
            })
            .to_string(),
        )
        .expect("write legacy oauth file");

        assert_eq!(
            load_oauth_credentials().expect("load legacy credentials"),
            Some(OAuthTokenSet {
                access_token: "legacy-access".to_string(),
                refresh_token: Some("legacy-refresh".to_string()),
                expires_at: Some(456),
                scopes: vec!["scope:legacy".to_string()],
            })
        );

        let store = super::test_system_store()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(store.contains_key(&system_store_key(&current_path)));
        drop(store);

        let legacy_contents =
            std::fs::read_to_string(&legacy_path).expect("legacy file should stay");
        assert!(legacy_contents.contains("\"other\": \"value\""));
        assert!(!legacy_contents.contains("\"oauth\""));

        std::env::remove_var("OPENYAK_CONFIG_HOME");
        clear_test_system_store();
        cleanup_dir_if_exists(&config_home);
        cleanup_dir_if_exists(
            legacy_path
                .parent()
                .expect("legacy credentials path should have parent"),
        );
    }

    #[cfg(unix)]
    fn assert_private_file_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(not(unix))]
    fn assert_private_file_permissions(_path: &Path) {}

    #[test]
    fn parses_callback_query_and_target() {
        let params =
            parse_oauth_callback_query("code=abc123&state=state-1&error_description=needs%20login")
                .expect("parse query");
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("state-1"));
        assert_eq!(params.error_description.as_deref(), Some("needs login"));

        let params = parse_oauth_callback_request_target("/callback?code=abc&state=xyz")
            .expect("parse callback target");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert!(parse_oauth_callback_request_target("/wrong?code=abc").is_err());

        let params =
            parse_oauth_callback_input("https://console.test/oauth/callback?code=def&state=uvw")
                .expect("parse callback url");
        assert_eq!(params.code.as_deref(), Some("def"));
        assert_eq!(params.state.as_deref(), Some("uvw"));

        let params =
            parse_oauth_callback_input("code=ghi&state=rst").expect("parse callback query input");
        assert_eq!(params.code.as_deref(), Some("ghi"));
        assert_eq!(params.state.as_deref(), Some("rst"));
    }
}
