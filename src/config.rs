// Resolves the contact email Unpaywall's polite-pool policy requires.
//
// Precedence: --email flag > FERREF_EMAIL env var > `email` key in
// ~/.config/ferref/config.toml. Never hardcoded -- see DESIGN.md Phase 8.
//
// The config file reader below is deliberately NOT a TOML parser: DESIGN.md
// asks for exactly one key, not a general settings system, so this is a
// ~10-line `key = value` reader instead of a new dependency.

use std::path::PathBuf;

fn config_path() -> Option<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_dir.join("ferref").join("config.toml"))
}

// Reads `key = "value"` / `key = value` lines out of a config file body,
// returning the value of `email` if present. Blank lines and `#` comments
// are skipped; anything else that doesn't parse as `key = value` is ignored
// rather than treated as an error -- a missing/junk key just means "no
// email configured", not a broken config.
fn parse_email(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "email" {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if value.is_empty() {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

// A missing config file is not an error -- it just means no email came from
// this source, same as an unset env var.
fn read_config_email() -> Option<String> {
    use std::io::Read as _;

    let path = config_path()?;
    // Bounded like the network reads in doi.rs. This one is a local file the
    // user controls, so the risk is far lower -- but a one-key config has no
    // business being megabytes, and an unbounded read of something that might
    // be /dev/zero is a silly way to hang.
    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CONFIG_BYTES)
        .read_to_string(&mut contents)
        .ok()?;
    parse_email(&contents)
}

pub fn resolve_email(cli_email: Option<String>) -> Result<String, String> {
    if let Some(email) = cli_email {
        return Ok(email);
    }
    if let Ok(email) = std::env::var("FERREF_EMAIL") {
        if !email.is_empty() {
            return Ok(email);
        }
    }
    if let Some(email) = read_config_email() {
        return Ok(email);
    }
    Err(
        "no email configured for Unpaywall's polite-pool policy; set one of: \
         --email <addr>, the FERREF_EMAIL environment variable, or \
         `email = you@example.com` in ~/.config/ferref/config.toml"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_quoting_and_whitespace() {
        let cfg = r#"
            # a comment above the key
            email = "thos@example.com"

            other_key = ignored
        "#;
        assert_eq!(parse_email(cfg), Some("thos@example.com".to_string()));

        assert_eq!(
            parse_email("email = bare@example.com"),
            Some("bare@example.com".to_string())
        );
        assert_eq!(
            parse_email("  email   =   spaced@example.com  "),
            Some("spaced@example.com".to_string())
        );
        assert_eq!(
            parse_email("email = 'single@example.com'"),
            Some("single@example.com".to_string())
        );
        assert_eq!(parse_email("# email = commented@example.com"), None);
        assert_eq!(parse_email("junk line with no equals\nother = 1"), None);
        assert_eq!(parse_email(""), None);
        assert_eq!(parse_email("email = "), None);
    }
}
