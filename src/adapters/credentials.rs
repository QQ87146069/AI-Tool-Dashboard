use super::{
    quota::{Access, Endpoint, Issue},
    tool_home,
};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use directories::BaseDirs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};
use zeroize::Zeroizing;

pub(super) fn private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Issue> {
    let file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Issue::NotConfigured
        } else {
            Issue::Credentials
        }
    })?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Issue::Credentials)?;
    if bytes.len() > 256 * 1024 {
        return Err(Issue::Credentials);
    }
    serde_json::from_slice(&bytes).map_err(|_| Issue::Credentials)
}

#[derive(Deserialize)]
struct CodexAuth {
    auth_mode: Option<String>,
    tokens: Option<CodexTokens>,
}
#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct ZcodeSettings {
    #[serde(rename = "providerFamilyDomain")]
    family: Option<String>,
    #[serde(rename = "providerFamilyConnectionSelections")]
    selections: Option<Selections>,
}
#[derive(Deserialize)]
struct Selections {
    bigmodel: Option<Selection>,
}
#[derive(Deserialize)]
struct Selection {
    kind: Option<String>,
}
impl ZcodeSettings {
    fn supported(&self) -> bool {
        self.family.as_deref() == Some("bigmodel")
            && self
                .selections
                .as_ref()
                .and_then(|s| s.bigmodel.as_ref())
                .and_then(|s| s.kind.as_deref())
                == Some("individual-coding-plan")
    }
}

#[derive(Deserialize)]
struct ZcodeSlots {
    #[serde(rename = "oauth:active_provider")]
    active: Option<String>,
    #[serde(rename = "oauth:bigmodel:access_token")]
    access: Option<String>,
    #[serde(rename = "oauth:bigmodel:user_info")]
    profile: Option<String>,
}

pub(super) fn codex() -> Result<Access, Issue> {
    let home = tool_home("CODEX_HOME", ".codex").ok_or(Issue::NotConfigured)?;
    let auth: CodexAuth = private_json(&home.join("auth.json"))?;
    if auth.auth_mode.as_deref() != Some("chatgpt") {
        return Err(Issue::Unsupported);
    }
    let tokens = auth.tokens.ok_or(Issue::NotConfigured)?;
    let token = Zeroizing::new(tokens.access_token);
    check_token(&token)?;
    let account = Zeroizing::new(
        tokens
            .account_id
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .ok_or(Issue::Credentials)?,
    );
    Ok(Access {
        endpoint: Endpoint::Codex,
        authorization: Zeroizing::new(format!("Bearer {}", token.as_str())),
        context: Zeroizing::new(String::new()),
        account: Some(account),
    })
}

pub(super) fn zcode() -> Result<Access, Issue> {
    let home = tool_home("ZCODE_HOME", ".zcode")
        .ok_or(Issue::NotConfigured)?
        .join("v2");
    let settings: ZcodeSettings = private_json(&home.join("setting.json"))?;
    // Only this personal-account route has been verified on the installed client.
    if !settings.supported() {
        return Err(Issue::Unsupported);
    }
    let slots: ZcodeSlots = private_json(&home.join("credentials.json"))?;
    let key = zcode_key()?;
    let active = decrypt(slots.active.as_deref().ok_or(Issue::NotConfigured)?, &key)?;
    if active.as_str() != "bigmodel" {
        return Err(Issue::Unsupported);
    }
    // Deliberately ignore cached API keys and project-token issuance paths.
    let token = decrypt(slots.access.as_deref().ok_or(Issue::NotConfigured)?, &key)?;
    check_token(&token)?;
    let profile = decrypt(slots.profile.as_deref().ok_or(Issue::NotConfigured)?, &key)?;
    #[derive(Deserialize)]
    struct Identity {
        id: String,
    }
    let profile: Identity = serde_json::from_str(&profile).map_err(|_| Issue::Credentials)?;
    if profile.id.trim().is_empty() {
        return Err(Issue::Credentials);
    }
    let authorization = if token.starts_with("Bearer ") {
        token
    } else {
        Zeroizing::new(format!("Bearer {}", token.as_str()))
    };
    Ok(Access {
        endpoint: Endpoint::BigModel,
        authorization,
        account: None,
        context: Zeroizing::new(profile.id),
    })
}

fn check_token(token: &str) -> Result<(), Issue> {
    if token.trim().is_empty() || token.len() > 32768 || token.chars().any(char::is_control) {
        return Err(Issue::Credentials);
    }
    // Decode expiry only, never treat unverified claims as identity or authority.
    let raw = token.strip_prefix("Bearer ").unwrap_or(token);
    if let Some(payload) = raw
        .split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
    {
        let payload = Zeroizing::new(payload);
        #[derive(Deserialize)]
        struct Expiry {
            exp: Option<u64>,
        }
        if let Ok(expiry) = serde_json::from_slice::<Expiry>(&payload)
            && let Some(exp) = expiry.exp
            && exp
                <= std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
        {
            return Err(Issue::Credentials);
        }
    }
    Ok(())
}

fn zcode_key() -> Result<Zeroizing<[u8; 32]>, Issue> {
    let secret = match std::env::var("ZCODE_CREDENTIAL_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(secret) => Zeroizing::new(secret),
        None => {
            let platform = if cfg!(target_os = "windows") {
                "win32"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "linux"
            };
            let home = BaseDirs::new().ok_or(Issue::Credentials)?;
            let username = whoami::username().map_err(|_| Issue::Credentials)?;
            Zeroizing::new(format!(
                "zcode-credential-fallback:{platform}:{}:{username}",
                home.home_dir().display()
            ))
        }
    };
    Ok(Zeroizing::new(Sha256::digest(secret.as_bytes()).into()))
}

fn decrypt(raw: &str, key: &[u8; 32]) -> Result<Zeroizing<String>, Issue> {
    let Some(body) = raw.strip_prefix("enc:v1:") else {
        return Ok(Zeroizing::new(raw.into()));
    };
    let parts: Vec<_> = body.split('.').collect();
    if parts.len() != 3 {
        return Err(Issue::Credentials);
    }
    let iv = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|_| Issue::Credentials)?;
    let tag = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| Issue::Credentials)?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| Issue::Credentials)?;
    if iv.len() != 12 || tag.len() != 16 {
        return Err(Issue::Credentials);
    }
    ciphertext.extend_from_slice(&tag);
    let nonce: [u8; 12] = iv.try_into().map_err(|_| Issue::Credentials)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| Issue::Credentials)?;
    let plaintext = cipher
        .decrypt(&Nonce::from(nonce), ciphertext.as_ref())
        .map_err(|_| Issue::Credentials)?;
    String::from_utf8(plaintext)
        .map(Zeroizing::new)
        .map_err(|_| Issue::Credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_the_verified_bigmodel_personal_connection() {
        let settings: ZcodeSettings = serde_json::from_str(r#"{"providerFamilyDomain":"bigmodel","providerFamilyConnectionSelections":{"bigmodel":{"kind":"individual-coding-plan"}},"recentProjects":["ignored"]}"#).unwrap();
        assert!(settings.supported());
        for json in [
            r#"{"providerFamilyDomain":"zai","providerFamilyConnectionSelections":{"bigmodel":{"kind":"individual-coding-plan"}}}"#,
            r#"{"providerFamilyDomain":"bigmodel","providerFamilyConnectionSelections":{"bigmodel":{"kind":"team-coding-plan"}}}"#,
            "{}",
        ] {
            assert!(
                !serde_json::from_str::<ZcodeSettings>(json)
                    .unwrap()
                    .supported()
            );
        }
    }

    #[test]
    fn rejects_malformed_encryption_and_expired_or_unsafe_headers() {
        assert_eq!(
            decrypt("enc:v1:bad.bad.bad", &[0; 32]).err(),
            Some(Issue::Credentials)
        );
        assert_eq!(check_token("bad\r\nheader"), Err(Issue::Credentials));
        let expired = format!("test.{}.test", URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#));
        assert_eq!(check_token(&expired), Err(Issue::Credentials));
    }

    #[test]
    fn decrypts_aes_gcm_and_rejects_other_users_or_modified_data() {
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let encrypted = cipher
            .encrypt(&Nonce::from(nonce), b"synthetic-only".as_ref())
            .unwrap();
        let (body, tag) = encrypted.split_at(encrypted.len() - 16);
        let encoded = format!(
            "enc:v1:{}.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(body)
        );
        assert_eq!(decrypt(&encoded, &key).unwrap().as_str(), "synthetic-only");
        assert_eq!(decrypt(&encoded, &[8; 32]).err(), Some(Issue::Credentials));
    }
}
