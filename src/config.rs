//! Settings read from the environment at start.

use std::{env, fmt, net::SocketAddr, path::PathBuf};

pub struct Config {
    pub database_url: String,
    pub files_dir: PathBuf,
    pub listen: SocketAddr,
    /// Host names the MCP endpoint answers to. Anything else is refused, which
    /// is the transport's defence against DNS rebinding.
    pub allowed_hosts: Vec<String>,
    pub read_write: Token,
    pub read_only: Token,
}

/// A bearer token and the name its changes are recorded under.
pub struct Token {
    pub name: String,
    pub secret: String,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Shorter tokens are refused so that a placeholder never reaches production.
const MIN_TOKEN_LEN: usize = 32;

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let listen = optional("HOUSE_LISTEN", "0.0.0.0:8080");
        let read_write = token("HOUSE_TOKEN_RW")?;
        let read_only = token("HOUSE_TOKEN_RO")?;
        if read_write.secret == read_only.secret {
            return Err("HOUSE_TOKEN_RW and HOUSE_TOKEN_RO are the same token".into());
        }
        Ok(Self {
            database_url: required("DATABASE_URL")?,
            files_dir: optional("HOUSE_FILES_DIR", "/data/files").into(),
            listen: listen
                .parse()
                .map_err(|_| format!("HOUSE_LISTEN is not an address: {listen}"))?,
            allowed_hosts: optional("HOUSE_ALLOWED_HOSTS", "house.pozza,localhost,127.0.0.1")
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(String::from)
                .collect(),
            read_write,
            read_only,
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set"))
}

fn optional(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn token(prefix: &str) -> Result<Token, String> {
    let secret = required(prefix)?;
    if secret.len() < MIN_TOKEN_LEN {
        return Err(format!("{prefix} is shorter than {MIN_TOKEN_LEN} characters"));
    }
    let name = required(&format!("{prefix}_NAME"))?;
    if name.trim().is_empty() {
        return Err(format!("{prefix}_NAME is empty"));
    }
    Ok(Token { name, secret })
}
