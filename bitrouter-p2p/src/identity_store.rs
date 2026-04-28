use std::path::Path;

use iroh::SecretKey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityStoreError {
    #[error("creating identity directory {path}: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("reading secret key at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("writing secret key at {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("secret key at {0} is malformed; expected 64 lowercase hex chars")]
    Malformed(String),
}

const SECRET_FILE_NAME: &str = "secret.key";

pub fn load_or_create_secret_key(data_dir: &Path) -> Result<SecretKey, IdentityStoreError> {
    let path = data_dir.join(SECRET_FILE_NAME);
    if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|source| IdentityStoreError::Read {
            path: path.display().to_string(),
            source,
        })?;
        return parse_secret_key(raw.trim(), &path);
    }

    std::fs::create_dir_all(data_dir).map_err(|source| IdentityStoreError::CreateDir {
        path: data_dir.display().to_string(),
        source,
    })?;
    let key = SecretKey::generate(rand::rngs::OsRng);
    write_secret_key(&path, &key)?;
    Ok(key)
}

fn parse_secret_key(raw: &str, path: &Path) -> Result<SecretKey, IdentityStoreError> {
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(IdentityStoreError::Malformed(path.display().to_string()));
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in raw.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk)
            .map_err(|_| IdentityStoreError::Malformed(path.display().to_string()))?;
        bytes[index] = u8::from_str_radix(hex, 16)
            .map_err(|_| IdentityStoreError::Malformed(path.display().to_string()))?;
    }
    Ok(SecretKey::from_bytes(&bytes))
}

fn write_secret_key(path: &Path, key: &SecretKey) -> Result<(), IdentityStoreError> {
    let contents = format!("{}\n", hex_encode(&key.to_bytes()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| IdentityStoreError::Write {
                path: path.display().to_string(),
                source,
            })?;
        file.write_all(contents.as_bytes())
            .map_err(|source| IdentityStoreError::Write {
                path: path.display().to_string(),
                source,
            })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents.as_bytes()).map_err(|source| IdentityStoreError::Write {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
