//! Environment based configuration helpers.

use std::fmt::Display;
use std::str::FromStr;

/// Reads `key` from the environment, falling back to `default` when unset
/// or empty. Panics with a clear message on unparsable values.
pub fn env_or<T>(key: &str, default: T) -> T
where
    T: FromStr,
    T::Err: Display,
{
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse()
            .unwrap_or_else(|err| panic!("invalid value for {key}={value:?}: {err}")),
        _ => default,
    }
}

pub fn env_string(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

pub fn env_flag(key: &str) -> bool {
    matches!(
        env_string(key, "0").to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn database_url() -> String {
    env_string("DATABASE_URL", "postgres://tlb:tlb@postgres:5432/tlb")
}
