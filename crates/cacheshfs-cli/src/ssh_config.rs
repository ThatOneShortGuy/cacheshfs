//! Minimal OpenSSH client-config (`~/.ssh/config`) resolution.
//!
//! We only need the handful of keywords that affect how the SFTP transport
//! connects — `HostName`, `User`, `Port`, and `IdentityFile` — so this is a
//! deliberately small parser rather than a full `ssh_config` implementation.
//! `Host` blocks with `*`/`?`/`!` patterns are honored with OpenSSH's
//! first-value-wins precedence; `Match` blocks are not supported and simply
//! deactivate until the next `Host` line so their settings are never
//! mis-applied.

use std::path::PathBuf;

/// Values resolved from an OpenSSH client config for a given host alias.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SshHostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<PathBuf>,
}

/// The default OpenSSH client config path (`~/.ssh/config`), if a home
/// directory can be determined.
pub fn default_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh").join("config"))
}

/// Resolve the effective settings for `alias` from the text of an ssh config.
///
/// Keywords are matched case-insensitively and the first value seen for a
/// keyword wins (as in OpenSSH), except `IdentityFile`, which accumulates.
/// Settings before the first `Host` line apply globally.
pub fn resolve(config: &str, alias: &str) -> SshHostConfig {
    let mut result = SshHostConfig::default();
    // Settings before any `Host` line apply to every host.
    let mut active = true;

    for raw in config.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((keyword, value)) = split_keyword(line) else {
            continue;
        };
        let keyword = keyword.to_ascii_lowercase();

        match keyword.as_str() {
            "host" => {
                active = host_patterns_match(value, alias);
            }
            // `Match` blocks are not supported; deactivate so we never apply
            // settings from a block whose condition we can't evaluate.
            "match" => {
                active = false;
            }
            _ if !active => {}
            "hostname" => {
                if result.hostname.is_none() {
                    result.hostname = Some(unquote(value));
                }
            }
            "user" => {
                if result.user.is_none() {
                    result.user = Some(unquote(value));
                }
            }
            "port" => {
                if result.port.is_none()
                    && let Ok(port) = unquote(value).parse::<u16>()
                {
                    result.port = Some(port);
                }
            }
            "identityfile" => {
                result.identity_files.push(expand_tilde(&unquote(value)));
            }
            _ => {}
        }
    }

    result
}

/// Drop a trailing/leading `#` comment. OpenSSH only treats `#` as a comment
/// when it begins a token, so we split on the first whitespace-preceded `#`.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        // A `#` at the very start, or after whitespace, begins a comment.
        Some(0) => "",
        Some(index) if line.as_bytes()[index - 1].is_ascii_whitespace() => &line[..index],
        _ => line,
    }
}

/// Split a config line into its keyword and value. The separator is either
/// whitespace or `=` (OpenSSH accepts both, e.g. `Port 22` or `Port=22`).
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let separator = line.find(|c: char| c == '=' || c.is_whitespace())?;
    let keyword = &line[..separator];
    if keyword.is_empty() {
        return None;
    }
    let value = line[separator..]
        .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
        .trim();
    Some((keyword, value))
}

/// Whether a `Host` line's space-separated patterns match `alias`. A negated
/// pattern (`!pat`) that matches vetoes the whole line; otherwise at least one
/// positive pattern must match.
fn host_patterns_match(patterns: &str, alias: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_match(negated, alias) {
                return false;
            }
        } else if glob_match(pattern, alias) {
            matched = true;
        }
    }
    matched
}

/// Match an OpenSSH host pattern (`*` = any run, `?` = one char) against text.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    fn matches(pattern: &[char], text: &[char]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some('*') => {
                matches(&pattern[1..], text) || (!text.is_empty() && matches(pattern, &text[1..]))
            }
            Some('?') => !text.is_empty() && matches(&pattern[1..], &text[1..]),
            Some(&c) => !text.is_empty() && text[0] == c && matches(&pattern[1..], &text[1..]),
        }
    }

    matches(&pattern, &text)
}

/// Strip surrounding double quotes from a config value, if present.
fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Expand a leading `~` (or `~/`) to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_simple_host_block() {
        let config = "\
Host server
    HostName home.example.com
    User braxton
    Port 2222
    IdentityFile /keys/id_ed25519
";
        let resolved = resolve(config, "server");
        assert_eq!(resolved.hostname.as_deref(), Some("home.example.com"));
        assert_eq!(resolved.user.as_deref(), Some("braxton"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(resolved.identity_files, vec![PathBuf::from("/keys/id_ed25519")]);
    }

    #[test]
    fn non_matching_alias_gets_nothing() {
        let config = "Host server\n    HostName home.example.com\n";
        assert_eq!(resolve(config, "other"), SshHostConfig::default());
    }

    #[test]
    fn first_value_wins_and_later_blocks_do_not_override() {
        let config = "\
Host server
    HostName first.example.com
Host *
    HostName wildcard.example.com
    User fallback
";
        let resolved = resolve(config, "server");
        // The specific block set HostName first, so the wildcard cannot override.
        assert_eq!(resolved.hostname.as_deref(), Some("first.example.com"));
        // But User was only set by the wildcard, so it still applies.
        assert_eq!(resolved.user.as_deref(), Some("fallback"));
    }

    #[test]
    fn wildcard_and_question_patterns_match() {
        let config = "Host *.example.com\n    User web\nHost db?\n    User dba\n";
        assert_eq!(resolve(config, "api.example.com").user.as_deref(), Some("web"));
        assert_eq!(resolve(config, "db1").user.as_deref(), Some("dba"));
        assert!(resolve(config, "db12").user.is_none());
    }

    #[test]
    fn negated_pattern_vetoes_a_match() {
        let config = "Host * !secret\n    User general\n";
        assert_eq!(resolve(config, "public").user.as_deref(), Some("general"));
        assert!(resolve(config, "secret").user.is_none());
    }

    #[test]
    fn equals_separator_and_comments_and_quotes() {
        let config = "\
# a comment
Host=server   # trailing comment
    HostName = \"home.example.com\"
    Port=22
";
        let resolved = resolve(config, "server");
        assert_eq!(resolved.hostname.as_deref(), Some("home.example.com"));
        assert_eq!(resolved.port, Some(22));
    }

    #[test]
    fn multiple_identity_files_accumulate() {
        let config = "Host server\n    IdentityFile /a\n    IdentityFile /b\n";
        let resolved = resolve(config, "server");
        assert_eq!(
            resolved.identity_files,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn settings_before_first_host_apply_globally() {
        let config = "User global\nHost server\n    Port 2222\n";
        let resolved = resolve(config, "server");
        assert_eq!(resolved.user.as_deref(), Some("global"));
        assert_eq!(resolved.port, Some(2222));
    }

    #[test]
    fn match_block_settings_are_ignored() {
        let config = "Match host server\n    User matched\n";
        // We do not evaluate Match conditions, so nothing is applied.
        assert!(resolve(config, "server").user.is_none());
    }
}
