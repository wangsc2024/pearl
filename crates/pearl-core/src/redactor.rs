//! # Secret Redaction
//!
//! 系統開發需求書 §60 — Security: secret redaction in log output.
//!
//! The `SecretRedactor` masks patterns that look like API keys, tokens, passwords,
//! and other secrets in text output before it reaches logs or terminal display.
//! This provides a defense-in-depth layer: even if a script accidentally prints
//! credentials to stdout, the redactor prevents them from persisting in logs.

/// The replacement text used when a secret is detected.
const REDACTED: &str = "[REDACTED]";

/// A pattern-based secret redactor.
///
/// Detects and masks common secret patterns in text:
/// - API keys (prefixed with sk-, pk-, api-, key-)
/// - Bearer tokens
/// - AWS-style credentials
/// - Generic password assignments
/// - Base64-encoded long tokens
/// - GitHub tokens (ghp_, gho_, ghs_, ghr_)
#[derive(Debug, Clone)]
pub struct SecretRedactor {
    /// Minimum length to consider a hex/base64 string as a potential secret.
    min_token_length: usize,
}

impl Default for SecretRedactor {
    fn default() -> Self {
        Self {
            min_token_length: 20,
        }
    }
}

impl SecretRedactor {
    /// Create a new redactor with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a redactor with a custom minimum token length threshold.
    pub fn with_min_token_length(min_token_length: usize) -> Self {
        Self { min_token_length }
    }

    /// Redact secrets from the given text, returning a sanitized version.
    ///
    /// This scans the text for known secret patterns and replaces them with
    /// `[REDACTED]`. The original text structure is preserved (newlines, etc.)
    /// so log formatting remains readable.
    pub fn redact(&self, input: &str) -> String {
        let mut output = input.to_string();

        // Order matters: more specific patterns first, then generic ones.
        output = self.redact_bearer_tokens(&output);
        output = self.redact_prefixed_keys(&output);
        output = self.redact_github_tokens(&output);
        output = self.redact_aws_keys(&output);
        output = self.redact_password_assignments(&output);
        output = self.redact_generic_long_tokens(&output);

        output
    }

    /// Redact Bearer token patterns: `Bearer <token>`
    fn redact_bearer_tokens(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut remaining = input;

        while let Some(pos) = remaining.to_lowercase().find("bearer ") {
            result.push_str(&remaining[..pos]);
            let after_bearer = &remaining[pos + 7..];
            // Find the end of the token (whitespace or end of string).
            let token_end = after_bearer
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(after_bearer.len());
            let token = &after_bearer[..token_end];
            if token.len() >= 10 {
                result.push_str("Bearer ");
                result.push_str(REDACTED);
            } else {
                result.push_str(&remaining[pos..pos + 7 + token_end]);
            }
            remaining = &after_bearer[token_end..];
        }
        result.push_str(remaining);
        result
    }

    /// Redact keys with known prefixes: sk-, pk-, api-, key-, token-
    fn redact_prefixed_keys(&self, input: &str) -> String {
        let prefixes = [
            "sk-", "pk-", "api-", "key-", "token-", "sk_live_", "sk_test_",
        ];
        let mut output = input.to_string();

        for prefix in &prefixes {
            let mut result = String::with_capacity(output.len());
            let mut remaining = output.as_str();

            while let Some(pos) = remaining.find(prefix) {
                // Check this is a word boundary (not mid-word).
                let is_boundary =
                    pos == 0 || !remaining.as_bytes()[pos - 1].is_ascii_alphanumeric();

                if !is_boundary {
                    result.push_str(&remaining[..pos + prefix.len()]);
                    remaining = &remaining[pos + prefix.len()..];
                    continue;
                }

                result.push_str(&remaining[..pos]);
                let after_prefix = &remaining[pos + prefix.len()..];
                let token_end = after_prefix
                    .find(|c: char| {
                        c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == ';'
                    })
                    .unwrap_or(after_prefix.len());
                let token = &after_prefix[..token_end];

                if token.len() >= 8 {
                    result.push_str(REDACTED);
                } else {
                    result.push_str(&remaining[pos..pos + prefix.len() + token_end]);
                }
                remaining = &after_prefix[token_end..];
            }
            result.push_str(remaining);
            output = result;
        }
        output
    }

    /// Redact GitHub tokens: ghp_, gho_, ghs_, ghr_
    fn redact_github_tokens(&self, input: &str) -> String {
        let prefixes = ["ghp_", "gho_", "ghs_", "ghr_"];
        let mut output = input.to_string();

        for prefix in &prefixes {
            let mut result = String::with_capacity(output.len());
            let mut remaining = output.as_str();

            while let Some(pos) = remaining.find(prefix) {
                result.push_str(&remaining[..pos]);
                let after_prefix = &remaining[pos + prefix.len()..];
                let token_end = after_prefix
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .unwrap_or(after_prefix.len());

                if token_end >= 10 {
                    result.push_str(REDACTED);
                } else {
                    result.push_str(&remaining[pos..pos + prefix.len() + token_end]);
                }
                remaining = &after_prefix[token_end..];
            }
            result.push_str(remaining);
            output = result;
        }
        output
    }

    /// Redact AWS-style access keys (AKIA...).
    fn redact_aws_keys(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut remaining = input;

        while let Some(pos) = remaining.find("AKIA") {
            result.push_str(&remaining[..pos]);
            let after = &remaining[pos..];
            // AWS keys are exactly 20 chars starting with AKIA.
            let key_end = after
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(after.len());
            if key_end >= 16 {
                result.push_str(REDACTED);
            } else {
                result.push_str(&after[..key_end]);
            }
            remaining = &after[key_end..];
        }
        result.push_str(remaining);
        result
    }

    /// Redact password= or password: assignments.
    fn redact_password_assignments(&self, input: &str) -> String {
        let patterns = [
            "password=",
            "password:",
            "passwd=",
            "passwd:",
            "secret=",
            "secret:",
        ];
        let mut output = input.to_string();

        for pattern in &patterns {
            let mut result = String::with_capacity(output.len());
            let lower = output.to_lowercase();
            let mut last_end = 0;

            let mut search_from = 0;
            while let Some(pos) = lower[search_from..].find(pattern) {
                let abs_pos = search_from + pos;
                result.push_str(&output[last_end..abs_pos + pattern.len()]);
                let after = &output[abs_pos + pattern.len()..];
                // Skip optional whitespace and quotes.
                let trimmed = after.trim_start();
                let skip = after.len() - trimmed.len();
                let value_start = if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                    skip + 1
                } else {
                    skip
                };
                result.push_str(&after[..value_start]);
                let value_text = &after[value_start..];
                let value_end = value_text
                    .find(|c: char| {
                        c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == ';'
                    })
                    .unwrap_or(value_text.len());
                if value_end >= 4 {
                    result.push_str(REDACTED);
                } else {
                    result.push_str(&value_text[..value_end]);
                }
                last_end = abs_pos + pattern.len() + value_start + value_end;
                search_from = last_end;
            }
            result.push_str(&output[last_end..]);
            output = result;
        }
        output
    }

    /// Redact generic long hex/base64 tokens (likely secrets).
    fn redact_generic_long_tokens(&self, input: &str) -> String {
        let mut result = String::new();
        for line in input.lines() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&self.redact_tokens_in_line(line));
        }
        // Preserve trailing newline if original had it.
        if input.ends_with('\n') {
            result.push('\n');
        }
        result
    }

    fn redact_tokens_in_line(&self, line: &str) -> String {
        let mut result = String::with_capacity(line.len());
        let mut chars = line.char_indices().peekable();

        while let Some((i, c)) = chars.next() {
            // Look for sequences that are purely hex or base64 charset and very long.
            if c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' {
                let start = i;
                let mut end = i + c.len_utf8();
                while let Some(&(_, nc)) = chars.peek() {
                    if nc.is_ascii_alphanumeric()
                        || nc == '+'
                        || nc == '/'
                        || nc == '='
                        || nc == '_'
                        || nc == '-'
                    {
                        end += nc.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                let token = &line[start..end];
                // Only redact very long tokens that look like secrets (40+ chars,
                // mostly alphanumeric with some base64 chars).
                if token.len() >= self.min_token_length + 20 && self.looks_like_secret_token(token)
                {
                    result.push_str(REDACTED);
                } else {
                    result.push_str(token);
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Heuristic: does this look like a secret token?
    ///
    /// True if the string has high entropy characteristics: mixed case, digits,
    /// and special base64 characters.
    fn looks_like_secret_token(&self, token: &str) -> bool {
        if token.len() < self.min_token_length + 20 {
            return false;
        }
        let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
        let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        // Must have at least two of: uppercase, lowercase, digits.
        let variety = has_upper as u8 + has_lower as u8 + has_digit as u8;
        variety >= 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_token() {
        let r = SecretRedactor::new();
        let input = "Authorization: Bearer sk_live_a1b2c3d4e5f6g7h8i9j0";
        let output = r.redact(input);
        assert!(!output.contains("a1b2c3d4e5"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_prefixed_api_key() {
        let r = SecretRedactor::new();
        let input = "Using key: sk-abcdefghij1234567890";
        let output = r.redact(input);
        assert!(!output.contains("abcdefghij"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_github_token() {
        let r = SecretRedactor::new();
        let input = "GITHUB_TOKEN=ghp_1234567890abcdef1234567890abcdef12";
        let output = r.redact(input);
        assert!(!output.contains("1234567890abcdef"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_aws_access_key() {
        let r = SecretRedactor::new();
        let input = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let output = r.redact(input);
        assert!(!output.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_password_assignment() {
        let r = SecretRedactor::new();
        let input = "password=mysupersecretpassword123";
        let output = r.redact(input);
        assert!(!output.contains("mysupersecretpassword"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn preserves_short_non_secrets() {
        let r = SecretRedactor::new();
        let input = "task_id=abc-123, status=ok";
        let output = r.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn preserves_normal_log_output() {
        let r = SecretRedactor::new();
        let input =
            "2024-01-01T00:00:00Z INFO task completed successfully\nresult: 42 items processed";
        let output = r.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redacts_multiple_secrets_in_one_line() {
        let r = SecretRedactor::new();
        let input = "api-key123456789012 and token-abcdefghijklmnop";
        let output = r.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("key123456789012"));
        assert!(!output.contains("abcdefghijklmnop"));
    }

    #[test]
    fn handles_empty_input() {
        let r = SecretRedactor::new();
        assert_eq!(r.redact(""), "");
    }

    #[test]
    fn preserves_multiline_structure() {
        let r = SecretRedactor::new();
        let input = "line1\npassword=secret1234\nline3\n";
        let output = r.redact(input);
        assert_eq!(output.lines().count(), 3);
        assert!(output.ends_with('\n'));
    }
}
