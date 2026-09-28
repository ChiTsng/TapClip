//! User configuration: `%APPDATA%\TapClip\config.toml`, created on first run
//! with a random pairing token.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

pub const DEFAULT_PORT: u16 = 47623;
pub const DEFAULT_TTL_SECONDS: u64 = 120;

/// Crockford base32: no I, L, O, U, so the token survives OCR and hand typing.
const TOKEN_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TOKEN_CHARS: usize = 16; // 80 bits

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub token: String,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_ttl() -> u64 {
    DEFAULT_TTL_SECONDS
}

pub fn default_path() -> PathBuf {
    app_dir("APPDATA").join("config.toml")
}

pub fn log_path() -> PathBuf {
    app_dir("LOCALAPPDATA").join("tapclip.log")
}

fn app_dir(env: &str) -> PathBuf {
    std::env::var_os(env).map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("TapClip")
}

/// Loads the config, creating it with a fresh token when missing.
/// Returns whether it was just created (first run).
pub fn load_or_create(path: &Path) -> Result<(Config, bool), String> {
    match fs::read_to_string(path) {
        Ok(raw) => parse(&raw).map(|c| (c, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let config = Config { port: DEFAULT_PORT, token: generate_token()?, ttl_seconds: DEFAULT_TTL_SECONDS };
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
            }
            fs::write(path, render(&config)).map_err(|e| format!("无法写入 {}：{e}", path.display()))?;
            Ok((config, true))
        }
        Err(e) => Err(format!("无法读取 {}：{e}", path.display())),
    }
}

pub fn parse(raw: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(raw).map_err(|e| format!("配置格式错误：{e}"))?;
    if config.port == 0 {
        return Err("port 不能为 0".into());
    }
    if !(10..=3600).contains(&config.ttl_seconds) {
        return Err("ttl_seconds 需要在 10 到 3600 之间".into());
    }
    if normalize_token(&config.token).len() < TOKEN_CHARS {
        return Err(format!("token 至少需要 {TOKEN_CHARS} 个字符"));
    }
    Ok(config)
}

fn render(config: &Config) -> String {
    format!(
        "# TapClip 配置。修改后重启 TapClip 生效。\n\
         \n\
         # 局域网端口，iPhone 快捷指令的地址里要用到\n\
         port = {}\n\
         \n\
         # 配对密钥，填进 iPhone 快捷指令（不区分大小写，短横线可有可无）\n\
         token = \"{}\"\n\
         \n\
         # 共享内容的有效时间（秒）\n\
         ttl_seconds = {}\n",
        config.port, config.token, config.ttl_seconds
    )
}

fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; TOKEN_CHARS];
    // SAFETY: the buffer is valid for its full length.
    let status = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if status != 0 {
        return Err(format!("无法生成随机密钥（NTSTATUS {status:#x}）"));
    }
    Ok(format_token(&bytes))
}

/// One base32 symbol per byte (low 5 bits), grouped as XXXX-XXXX-XXXX-XXXX.
fn format_token(bytes: &[u8; TOKEN_CHARS]) -> String {
    let mut out = String::with_capacity(TOKEN_CHARS + 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(TOKEN_ALPHABET[(b & 31) as usize] as char);
    }
    out
}

/// Canonical token form for comparison: separators dropped, upper case, and
/// Crockford's look-alike letters mapped back to digits.
pub fn normalize_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_and_normalization() {
        let token = format_token(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 31]);
        assert_eq!(token, "0123-4567-89AB-CDEZ");
        assert_eq!(normalize_token("o123-4567-89ab-cdez"), "0123456789ABCDEZ");
        assert_eq!(normalize_token(" 0I23 4567 89AB CDEZ "), "0123456789ABCDEZ");
    }

    #[test]
    fn generated_tokens_are_random() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(normalize_token(&a).len(), TOKEN_CHARS);
    }

    #[test]
    fn rendered_config_round_trips() {
        let config = Config { port: 5000, token: "ABCD-EFGH-JKMN-PQRS".into(), ttl_seconds: 90 };
        let parsed = parse(&render(&config)).unwrap();
        assert_eq!((parsed.port, parsed.token.as_str(), parsed.ttl_seconds), (5000, "ABCD-EFGH-JKMN-PQRS", 90));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse("token = \"short\"").is_err());
        assert!(parse("token = \"ABCD-EFGH-JKMN-PQRS\"\nttl_seconds = 5").is_err());
        assert!(parse("token = \"ABCD-EFGH-JKMN-PQRS\"\nport = 0").is_err());
        assert!(parse("token = \"ABCD-EFGH-JKMN-PQRS\"\ntypo = 1").is_err());
        assert!(parse("token = \"ABCD-EFGH-JKMN-PQRS\"").is_ok());
    }
}
