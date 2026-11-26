//! Snowflake `AI_EMBED` as a cloud embedding backend.
//!
//! memb signs in with a key pair (a short-lived RS256 JWT built from an unencrypted PKCS#8
//! `.p8` key that stays in a local file) and sends one SQL statement per batch to the
//! account's SQL API over HTTPS. The request goes through Windows' own `curl.exe`, so memb
//! needs no HTTP or TLS library. The key text, the token and the embedded text never appear
//! in an error message.
//!
//! This sends the text it embeds (note titles, descriptions, search queries) to Snowflake.
//! It is only used when the user has configured it.
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use ring::digest;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Texts per statement. Each text is a bind variable, so this also bounds the binding count.
pub const BATCH: usize = 16;
const TOKEN_LIFETIME_SECS: u64 = 55 * 60;
const MAX_TEXT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct SnowflakeConfig {
    /// Account identifier, for example `ab14212.ap-southeast-1` (locator with region).
    pub account: String,
    pub user: String,
    pub role: Option<String>,
    pub warehouse: String,
    /// The AI_EMBED model, for example `snowflake-arctic-embed-m-v1.5`.
    pub model: String,
    pub key_path: PathBuf,
    /// Tests only: an `http://127.0.0.1:<port>` base URL instead of the account's host.
    pub base_url: Option<String>,
}

fn only(text: &str, extra: &str, what: &str) -> Result<(), String> {
    if text.is_empty() || text.len() > 128 || !text.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c)) {
        return Err(format!("snowflake {what} is empty, too long or has characters that are not allowed"));
    }
    Ok(())
}

impl SnowflakeConfig {
    pub fn validate(&self) -> Result<(), String> {
        only(&self.account, ".-_", "account")?;
        only(&self.user, "._-$@", "user")?;
        only(&self.warehouse, "_$", "warehouse")?;
        only(&self.model, ".-_", "model")?;
        if let Some(role) = &self.role {
            only(role, "_$", "role")?;
        }
        if let Some(base) = &self.base_url {
            // Exactly `http://127.0.0.1:<port>`: anything after the port (a path, or `@host`
            // userinfo that would turn the real host into something else) is refused.
            let port = base.strip_prefix("http://127.0.0.1:").unwrap_or("");
            if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u32>().map_or(true, |p| p == 0 || p > 65535) {
                return Err("snowflake base_url is only accepted as http://127.0.0.1:<port> for a loopback test server".into());
            }
        }
        Ok(())
    }

    fn host_url(&self) -> String {
        match &self.base_url {
            Some(base) => base.clone(),
            None => format!("https://{}.snowflakecomputing.com", self.account.to_ascii_lowercase()),
        }
    }

    /// The account part of the JWT claims: the locator without region, upper case.
    fn jwt_account(&self) -> String {
        self.account.split('.').next().unwrap_or(&self.account).to_ascii_uppercase()
    }
}

/// The model used when none is configured: the best balance measured on this project's
/// synthetic corpus (see evidence/t031-snowflake-ai-embed.md): fastest and best with the retrieval prefix.
pub const DEFAULT_MODEL: &str = "snowflake-arctic-embed-m-v1.5";

/// Whether and how a query is prefixed. Measured with the project's own test corpus: the
/// arctic-embed-m models rank best with the retrieval instruction in front of the query,
/// arctic-embed-l-v2.0 ranks best (and much worse with the instruction) without it.
/// Models that were not measured get no prefix.
pub fn query_prefix(model: &str) -> &'static str {
    if model.starts_with("snowflake-arctic-embed-m") {
        "Represent this sentence for searching relevant passages: "
    } else {
        ""
    }
}

/// What a note is embedded as (the form that was measured).
pub fn document_text(title: &str, desc: &str) -> String {
    format!("{title}: {desc}")
}

/// The identity vectors made through this backend carry: model and prompt policy, so
/// vectors from another model or another prompt are never compared with these.
pub fn identity(model: &str) -> String {
    let prompt = if query_prefix(model).is_empty() { "plain" } else { "retrieval-prefix" };
    format!("snowflake|{model}|prompt:{prompt}-v1")
}

/// The configuration from `MEMB_SNOWFLAKE_*` (config.toml sets these when they are unset).
/// `Ok(None)` means Snowflake is not configured (no account). A partial setup is an error
/// naming the missing setting, never a silent fallback to something else.
pub fn config_from_env() -> Result<Option<SnowflakeConfig>, String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(account) = var("MEMB_SNOWFLAKE_ACCOUNT") else { return Ok(None) };
    let need = |k: &str| var(k).ok_or_else(|| format!("{k} is not set (Snowflake needs an account, user, warehouse and key file)"));
    let cfg = SnowflakeConfig {
        account,
        user: need("MEMB_SNOWFLAKE_USER")?,
        role: var("MEMB_SNOWFLAKE_ROLE"),
        warehouse: need("MEMB_SNOWFLAKE_WAREHOUSE")?,
        model: var("MEMB_SNOWFLAKE_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        key_path: PathBuf::from(need("MEMB_SNOWFLAKE_KEY")?),
        base_url: var("MEMB_SNOWFLAKE_BASE_URL"),
    };
    cfg.validate()?;
    Ok(Some(cfg))
}

// ---- small encoders (kept local so memb needs no extra crate)

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8], url_safe: bool) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                let c = B64[((n >> (18 - 6 * i)) & 63) as usize] as char;
                out.push(match (url_safe, c) {
                    (true, '+') => '-',
                    (true, '/') => '_',
                    _ => c,
                });
            } else if !url_safe {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = B64.iter().position(|b| *b == c).ok_or("invalid base64 in the key file")? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn der_len(n: usize) -> Vec<u8> {
    if n < 128 {
        vec![n as u8]
    } else if n < 256 {
        vec![0x81, n as u8]
    } else {
        vec![0x82, (n >> 8) as u8, n as u8]
    }
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(der_len(content.len()));
    out.extend_from_slice(content);
    out
}

/// `SHA256:<base64>` of the DER SubjectPublicKeyInfo of an RSA public key (PKCS#1 DER in),
/// the fingerprint Snowflake shows for a registered key.
pub fn public_key_fingerprint(rsa_public_key_der: &[u8]) -> String {
    // AlgorithmIdentifier { rsaEncryption, NULL }
    let algorithm: [u8; 15] = [0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00];
    let mut bit_string = vec![0u8];
    bit_string.extend_from_slice(rsa_public_key_der);
    let mut body = algorithm.to_vec();
    body.extend(der(0x03, &bit_string));
    let spki = der(0x30, &body);
    format!("SHA256:{}", base64_encode(digest::digest(&digest::SHA256, &spki).as_ref(), false))
}

/// Reads an unencrypted PKCS#8 PEM private key. Never echoes any of its text.
pub fn load_key(path: &Path) -> Result<RsaKeyPair, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read the Snowflake key file {}: {}", path.display(), e.kind()))?;
    if text.contains("BEGIN ENCRYPTED PRIVATE KEY") {
        return Err("the Snowflake key is encrypted; memb needs an unencrypted PKCS#8 key (BEGIN PRIVATE KEY)".into());
    }
    let begin = "-----BEGIN PRIVATE KEY-----";
    let end = "-----END PRIVATE KEY-----";
    let start = text.find(begin).ok_or("the Snowflake key file is not a PKCS#8 PEM (BEGIN PRIVATE KEY)")? + begin.len();
    let stop = text[start..].find(end).ok_or("the Snowflake key file has no END PRIVATE KEY line")? + start;
    let der = base64_decode(&text[start..stop])?;
    RsaKeyPair::from_pkcs8(&der).map_err(|_| "the Snowflake key is not a usable RSA PKCS#8 key (2048 bits or more)".to_string())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A signed key-pair JWT for the SQL API.
pub fn make_jwt(cfg: &SnowflakeConfig, key: &RsaKeyPair, issued_at: u64) -> Result<String, String> {
    let account = cfg.jwt_account();
    let user = cfg.user.to_ascii_uppercase();
    let qualified = format!("{account}.{user}");
    let header = base64_encode(br#"{"alg":"RS256","typ":"JWT"}"#, true);
    let claims = serde_json::json!({
        "iss": format!("{qualified}.{}", public_key_fingerprint(key.public().as_ref())),
        "sub": qualified,
        "iat": issued_at,
        "exp": issued_at + TOKEN_LIFETIME_SECS,
    });
    let signing_input = format!("{header}.{}", base64_encode(claims.to_string().as_bytes(), true));
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| "could not sign the Snowflake token".to_string())?;
    Ok(format!("{signing_input}.{}", base64_encode(&signature, true)))
}

/// One authenticated connection: the key is read once, the token renewed before it expires.
pub struct Client {
    cfg: SnowflakeConfig,
    key: RsaKeyPair,
    token: Mutex<Option<(String, u64)>>,
}

impl Client {
    pub fn new(cfg: SnowflakeConfig) -> Result<Self, String> {
        cfg.validate()?;
        let key = load_key(&cfg.key_path)?;
        Ok(Self { cfg, key, token: Mutex::new(None) })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    fn token(&self) -> Result<String, String> {
        let now = now_secs();
        let mut guard = self.token.lock().map_err(|_| "token lock poisoned".to_string())?;
        if let Some((jwt, issued)) = guard.as_ref() {
            if now < issued + TOKEN_LIFETIME_SECS - 300 {
                return Ok(jwt.clone());
            }
        }
        let jwt = make_jwt(&self.cfg, &self.key, now)?;
        *guard = Some((jwt.clone(), now));
        Ok(jwt)
    }

    /// Embeds `texts` (in order). Each batch is one SQL statement; vectors come back as
    /// they are (the model's own width and normalization).
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH) {
            out.extend(self.embed_chunk(chunk)?);
        }
        Ok(out)
    }

    fn embed_chunk(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.iter().any(|t| t.len() > MAX_TEXT_BYTES) {
            return Err("a text is too long to send to Snowflake".into());
        }
        // AI_EMBED needs the model as a string literal, not a bind variable; the name has
        // been checked to hold only letters, digits, dots, dashes and underscores.
        let mut statement = format!("SELECT AI_EMBED('{}', t.v) FROM (SELECT column1 AS i, column2 AS v FROM VALUES ", self.cfg.model);
        let mut bindings = serde_json::Map::new();
        for (i, text) in texts.iter().enumerate() {
            if i > 0 {
                statement.push_str(", ");
            }
            statement.push_str(&format!("({i}, ?)"));
            bindings.insert((i + 1).to_string(), serde_json::json!({"type": "TEXT", "value": text}));
        }
        statement.push_str(") t ORDER BY t.i");
        let mut body = serde_json::json!({
            "statement": statement,
            "timeout": 90,
            "warehouse": self.cfg.warehouse,
            "bindings": bindings,
        });
        if let Some(role) = &self.cfg.role {
            body["role"] = serde_json::Value::String(role.clone());
        }
        let jwt = self.token()?;
        let url = format!("{}/api/v2/statements", self.cfg.host_url());
        let (status, text) = curl(&url, "POST", &jwt, Some(&body.to_string()), self.cfg.base_url.is_some())?;
        let mut value = parse_response(status, &text)?;
        // A statement that is still running answers 202 with a handle: poll it.
        let deadline = Instant::now() + Duration::from_secs(120);
        while value.get("data").is_none() {
            let Some(path) = value.get("statementStatusUrl").and_then(|v| v.as_str()) else {
                return Err("Snowflake gave neither rows nor a status URL".into());
            };
            // Only a path under the statements API on the same host is ever followed.
            if !path.starts_with("/api/v2/statements/") || path.contains("..") || path.contains("//") || path.contains('@') {
                return Err("Snowflake gave an unusable status URL".into());
            }
            if Instant::now() > deadline {
                return Err("Snowflake did not finish the embedding statement in time".into());
            }
            std::thread::sleep(Duration::from_millis(500));
            let url = format!("{}{}", self.cfg.host_url(), path);
            let (status, text) = curl(&url, "GET", &jwt, None, self.cfg.base_url.is_some())?;
            value = parse_response(status, &text)?;
        }
        let mut rows = value["data"].as_array().ok_or("Snowflake rows were not a list")?.clone();
        // Large results (vectors are) arrive in partitions: the first is inline, the rest are fetched.
        let partitions = value["resultSetMetaData"]["partitionInfo"].as_array().map_or(1, Vec::len);
        if partitions > 1 {
            let handle = value["statementHandle"].as_str().ok_or("Snowflake gave partitions but no statement handle")?;
            if handle.is_empty() || !handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err("Snowflake gave an unusable statement handle".into());
            }
            for part in 1..partitions {
                let url = format!("{}/api/v2/statements/{handle}?partition={part}", self.cfg.host_url());
                let (status, text) = curl(&url, "GET", &jwt, None, self.cfg.base_url.is_some())?;
                let page = parse_response(status, &text)?;
                rows.extend(page["data"].as_array().ok_or("a Snowflake result partition had no rows")?.iter().cloned());
            }
        }
        if rows.len() != texts.len() {
            return Err(format!("Snowflake returned {} vectors for {} texts", rows.len(), texts.len()));
        }
        rows.iter().map(|row| parse_vector(&row[0])).collect()
    }
}

fn parse_response(status: u16, text: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    if (200..300).contains(&status) {
        return if value.is_null() { Err("Snowflake answered with something that is not JSON".into()) } else { Ok(value) };
    }
    // Only Snowflake's own error text, cut short; never the request.
    let message = value.get("message").and_then(|m| m.as_str()).unwrap_or("no message");
    let message: String = message.chars().take(300).collect();
    Err(format!("Snowflake refused the request (HTTP {status}): {message}"))
}

fn parse_vector(cell: &serde_json::Value) -> Result<Vec<f32>, String> {
    let owned;
    let array = match cell {
        serde_json::Value::String(s) => {
            owned = serde_json::from_str::<serde_json::Value>(s).map_err(|_| "a Snowflake vector was not readable".to_string())?;
            &owned
        }
        other => other,
    };
    let items = array.as_array().ok_or("a Snowflake vector was not a list")?;
    let v: Vec<f32> = items
        .iter()
        .map(|n| n.as_f64().filter(|x| x.is_finite()).map(|x| x as f32).ok_or("a Snowflake vector had a bad number"))
        .collect::<Result<_, _>>()?;
    if v.is_empty() || v.len() > 65536 {
        return Err("a Snowflake vector had an invalid size".into());
    }
    Ok(v)
}

/// Runs Windows' curl.exe with a one-off config file (so the token never appears on a
/// command line) and returns the HTTP status and body.
fn curl(url: &str, method: &str, jwt: &str, body: Option<&str>, loopback: bool) -> Result<(u16, String), String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let curl_exe = root.join("System32").join("curl.exe");
    if !curl_exe.is_file() {
        return Err(format!("Windows' curl.exe was not found at {}", curl_exe.display()));
    }
    let dir = std::env::temp_dir().join(format!("memb-sf-{}-{}", std::process::id(), COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create a temporary folder: {e}"))?;
    let result = (|| {
        let escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut config = format!(
            "url = \"{}\"\nrequest = \"{method}\"\nsilent\nshow-error\ncompressed\nmax-time = 150\nproto = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"X-Snowflake-Authorization-Token-Type: KEYPAIR_JWT\"\nheader = \"Accept: application/json\"\nheader = \"User-Agent: memb\"\nwrite-out = \"\\n%{{http_code}}\"\n",
            escape(url),
            if loopback { "=http" } else { "=https" },
            jwt
        );
        if let Some(body) = body {
            let path = dir.join("body.json");
            std::fs::write(&path, body).map_err(|e| format!("cannot write the request: {e}"))?;
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!("data-binary = \"@{}\"\n", escape(&path.to_string_lossy().replace('\\', "/"))));
        }
        let config_path = dir.join("curl.cfg");
        std::fs::write(&config_path, config).map_err(|e| format!("cannot write the request: {e}"))?;
        let out = Command::new(&curl_exe)
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("could not start curl.exe: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').ok_or_else(|| {
            let err = String::from_utf8_lossy(&out.stderr);
            format!("no answer from Snowflake ({})", err.lines().next().unwrap_or("curl failed").chars().take(200).collect::<String>())
        })?;
        let status = code.trim().parse::<u16>().map_err(|_| "no HTTP status from Snowflake".to_string())?;
        Ok((status, body.to_owned()))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A throwaway 2048-bit key made for these tests only; it protects nothing.
    const TEST_KEY: &str = include_str!("../tests/fixtures/snowflake_test_key.p8");
    /// SHA256 of its SubjectPublicKeyInfo, computed independently with OpenSSL.
    const TEST_FINGERPRINT: &str = "SHA256:0SYQrbK23JoWb2miRTvLDZvqFMidkZkPUDzit9g++Oo=";

    fn key_file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memb-sf-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("key.p8");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn config(key: PathBuf, base_url: Option<String>) -> SnowflakeConfig {
        SnowflakeConfig {
            account: "ab14212.ap-southeast-1".into(),
            user: "MCDACORTEX".into(),
            role: Some("SOME_ROLE".into()),
            warehouse: "SHARED_ID_XS".into(),
            model: "snowflake-arctic-embed-m-v1.5".into(),
            key_path: key,
            base_url,
        }
    }

    #[test]
    fn base64_matches_known_values_and_round_trips() {
        assert_eq!(base64_encode(b"", false), "");
        assert_eq!(base64_encode(b"f", false), "Zg==");
        assert_eq!(base64_encode(b"fo", false), "Zm8=");
        assert_eq!(base64_encode(b"foo", false), "Zm9v");
        assert_eq!(base64_encode(b"foobar", false), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xfe], false), "+//+");
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xfe], true), "-__-");
        assert_eq!(base64_encode(b"f", true), "Zg", "url-safe has no padding");
        for data in [&b""[..], b"a", b"ab", b"abc", &[0u8, 255, 128, 7, 9]] {
            assert_eq!(base64_decode(&base64_encode(data, false)).unwrap(), data);
        }
        assert!(base64_decode("!!!").is_err());
    }

    #[test]
    fn the_jwt_has_the_documented_claims_and_a_valid_signature() {
        let p = key_file("jwt", TEST_KEY);
        let cfg = config(p, None);
        let key = load_key(&cfg.key_path).unwrap();
        // The fingerprint equals the one OpenSSL computes for the same key.
        assert_eq!(public_key_fingerprint(key.public().as_ref()), TEST_FINGERPRINT);
        let jwt = make_jwt(&cfg, &key, 1_800_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let b64url = |s: &str| {
            let mut t = s.replace('-', "+").replace('_', "/");
            while t.len() % 4 != 0 {
                t.push('=');
            }
            base64_decode(&t).unwrap()
        };
        let header: serde_json::Value = serde_json::from_slice(&b64url(parts[0])).unwrap();
        assert_eq!(header["alg"], "RS256");
        let claims: serde_json::Value = serde_json::from_slice(&b64url(parts[1])).unwrap();
        assert_eq!(claims["sub"], "AB14212.MCDACORTEX", "locator without region, upper case");
        assert_eq!(claims["iss"], format!("AB14212.MCDACORTEX.{TEST_FINGERPRINT}"));
        assert_eq!((claims["iat"].as_u64(), claims["exp"].as_u64()), (Some(1_800_000_000), Some(1_800_000_000 + 55 * 60)));
        // The signature verifies against the public key, and not against a changed payload.
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
        let signed = format!("{}.{}", parts[0], parts[1]);
        assert!(public.verify(signed.as_bytes(), &b64url(parts[2])).is_ok());
        assert!(public.verify(format!("{}.x{}", parts[0], parts[1]).as_bytes(), &b64url(parts[2])).is_err());
        let _ = std::fs::remove_dir_all(cfg.key_path.parent().unwrap());
    }

    #[test]
    fn bad_keys_and_settings_are_refused_without_echoing_anything() {
        let encrypted = key_file("enc", "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n");
        assert!(load_key(&encrypted).unwrap_err().contains("encrypted"));
        let junk = key_file("junk", "-----BEGIN PRIVATE KEY-----\nc2VjcmV0LXRleHQ=\n-----END PRIVATE KEY-----\n");
        let err = load_key(&junk).err().unwrap();
        assert!(!err.contains("c2VjcmV0") && !err.contains("secret-text"), "{err}");
        let not_pem = key_file("nopem", "hello");
        assert!(load_key(&not_pem).is_err());
        assert!(load_key(Path::new("C:/definitely/not/here.p8")).unwrap_err().contains("cannot read"));
        let ok_key = key_file("cfg", TEST_KEY);
        let mut bad = config(ok_key.clone(), None);
        for mutate in [
            (|c: &mut SnowflakeConfig| c.account = "a b".into()) as fn(&mut SnowflakeConfig),
            |c| c.user = "x;DROP".into(),
            |c| c.warehouse = "w'h".into(),
            |c| c.model = "".into(),
            |c| c.role = Some("r r".into()),
            |c| c.base_url = Some("http://evil.example".into()),
            |c| c.base_url = Some("http://127.0.0.1:1@evil.example".into()),
            |c| c.base_url = Some("http://127.0.0.1:80/path".into()),
            |c| c.base_url = Some("http://127.0.0.1:".into()),
            |c| c.base_url = Some("http://127.0.0.1:0".into()),
            |c| c.base_url = Some("http://127.0.0.1:99999".into()),
            |c| c.base_url = Some("http://127.0.0.1:80.evil.example".into()),
        ] {
            bad = config(ok_key.clone(), None);
            mutate(&mut bad);
            assert!(bad.validate().is_err());
        }
        let _ = bad;
        assert!(config(ok_key, Some("http://127.0.0.1:1".into())).validate().is_ok());
    }

    /// A loopback stand-in for the SQL API. Records each request; answers with `reply`.
    fn serve(replies: Vec<(u16, String)>) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                seen.lock().unwrap().push(String::from_utf8_lossy(&request).into_owned());
                let (status, body) = replies.next().unwrap_or((500, "{}".into()));
                let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (port, log)
    }

    #[test]
    fn a_batch_is_one_statement_with_bound_text_and_vectors_come_back_in_order() {
        let key = key_file("batch", TEST_KEY);
        let reply = serde_json::json!({"data": [["[0.5, -0.25, 0.125]"], [[1.0, 2.0, 3.0]]]}).to_string();
        let (port, log) = serve(vec![(200, reply)]);
        let client = Client::new(config(key, Some(format!("http://127.0.0.1:{port}")))).unwrap();
        let texts = vec!["first text with 'quotes' and ; semicolon".to_string(), "second".to_string()];
        let vectors = client.embed_batch(&texts).unwrap();
        assert_eq!(vectors, vec![vec![0.5, -0.25, 0.125], vec![1.0, 2.0, 3.0]]);
        let request = log.lock().unwrap()[0].clone();
        assert!(request.starts_with("POST /api/v2/statements HTTP/1.1"), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer ey") && lower.contains("x-snowflake-authorization-token-type: keypair_jwt"));
        let body: serde_json::Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let statement = body["statement"].as_str().unwrap();
        assert!(statement.contains("AI_EMBED('snowflake-arctic-embed-m-v1.5', t.v)") && statement.contains("(0, ?), (1, ?)") && statement.ends_with("ORDER BY t.i"));
        assert!(!statement.contains("quotes") && !statement.contains("first text"), "text is bound, never spliced into SQL");
        assert_eq!(body["bindings"]["1"]["value"], texts[0]);
        assert_eq!(body["bindings"]["2"]["value"], "second");
        assert!(body["bindings"].get("3").is_none());
        assert_eq!((body["warehouse"].as_str(), body["role"].as_str()), (Some("SHARED_ID_XS"), Some("SOME_ROLE")));
    }

    #[test]
    fn errors_name_snowflakes_reason_but_never_the_token_the_key_or_the_text() {
        let key = key_file("err", TEST_KEY);
        let (port, _) = serve(vec![
            (401, serde_json::json!({"message": "JWT token is invalid."}).to_string()),
            (200, serde_json::json!({"data": [["[1.0]"]]}).to_string()),
            (200, "not json".into()),
        ]);
        let client = Client::new(config(key, Some(format!("http://127.0.0.1:{port}")))).unwrap();
        let err = client.embed_batch(&["confidential note body".into()]).unwrap_err();
        assert!(err.contains("HTTP 401") && err.contains("JWT token is invalid"), "{err}");
        assert!(!err.contains("confidential") && !err.contains("ey"), "{err}");
        // A wrong number of rows is an error, not a silent misalignment.
        let err = client.embed_batch(&["a".into(), "b".into()]).unwrap_err();
        assert!(err.contains("1 vectors for 2 texts"), "{err}");
        assert!(client.embed_batch(&["c".into()]).is_err());
        // Nothing listening: a clear failure.
        let dead = Client::new(config(key_file("dead", TEST_KEY), Some("http://127.0.0.1:1".into()))).unwrap();
        assert!(dead.embed_batch(&["x".into()]).is_err());
    }

    #[test]
    fn a_result_split_into_partitions_is_fetched_and_joined_in_order() {
        let key = key_file("parts", TEST_KEY);
        let first = serde_json::json!({
            "statementHandle": "01b2c3d4-0000-1111-2222-333344445555",
            "resultSetMetaData": {"numRows": 3, "partitionInfo": [{"rowCount": 2}, {"rowCount": 1}]},
            "data": [["[1.0]"], ["[2.0]"]]
        });
        let second = serde_json::json!({"data": [["[3.0]"]]});
        let (port, log) = serve(vec![(200, first.to_string()), (200, second.to_string())]);
        let client = Client::new(config(key, Some(format!("http://127.0.0.1:{port}")))).unwrap();
        let v = client.embed_batch(&["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(v, vec![vec![1.0], vec![2.0], vec![3.0]]);
        let requests = log.lock().unwrap().clone();
        assert!(requests[1].starts_with("GET /api/v2/statements/01b2c3d4-0000-1111-2222-333344445555?partition=1 HTTP/1.1"), "{}", requests[1]);
        // A handle that could smuggle a path or query is refused.
        let key = key_file("parts2", TEST_KEY);
        let evil = serde_json::json!({"statementHandle": "x/../../y?z=1", "resultSetMetaData": {"partitionInfo": [{}, {}]}, "data": [["[1.0]"]]});
        let (port, _) = serve(vec![(200, evil.to_string())]);
        let client = Client::new(config(key, Some(format!("http://127.0.0.1:{port}")))).unwrap();
        assert!(client.embed_batch(&["a".into(), "b".into()]).unwrap_err().contains("handle"));
    }

    #[test]
    fn a_status_url_outside_the_statements_api_is_never_followed() {
        let key = key_file("status", TEST_KEY);
        for bad in ["http://evil.example/x", "/api/v2/statements/../../x", "/other/path", "/api/v2/statements/a@evil"] {
            let reply = serde_json::json!({"statementStatusUrl": bad}).to_string();
            let (port, log) = serve(vec![(202, reply)]);
            let client = Client::new(config(key.clone(), Some(format!("http://127.0.0.1:{port}")))).unwrap();
            assert!(client.embed_batch(&["a".into()]).unwrap_err().contains("status URL"), "{bad}");
            assert_eq!(log.lock().unwrap().len(), 1, "no second request for {bad}");
        }
    }

    #[test]
    fn more_than_one_batch_is_split_and_a_long_text_is_refused() {
        let key = key_file("split", TEST_KEY);
        let first: Vec<serde_json::Value> = (0..BATCH).map(|i| serde_json::json!([[i as f64]])).collect();
        let second = vec![serde_json::json!([[99.0]])];
        let (port, log) = serve(vec![(200, serde_json::json!({"data": first}).to_string()), (200, serde_json::json!({"data": second}).to_string())]);
        let client = Client::new(config(key, Some(format!("http://127.0.0.1:{port}")))).unwrap();
        let texts: Vec<String> = (0..BATCH + 1).map(|i| format!("t{i}")).collect();
        let vectors = client.embed_batch(&texts).unwrap();
        assert_eq!(vectors.len(), BATCH + 1);
        assert_eq!((vectors[0][0], vectors[BATCH][0]), (0.0, 99.0));
        assert_eq!(log.lock().unwrap().len(), 2, "two statements");
        assert!(client.embed_batch(&["x".repeat(MAX_TEXT_BYTES + 1)]).is_err());
    }
}
