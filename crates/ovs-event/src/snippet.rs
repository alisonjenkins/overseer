use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub const SNIPPET_MAX_BYTES: usize = 2048;
const ELLIPSIS: char = '…';
const REDACTED: &str = "[redacted]";

struct Rule {
    pattern: &'static str,
    replacement: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        pattern: r"(?i)(authorization:\s*)(?:bearer\s+|basic\s+)?\S+",
        replacement: "${1}[redacted]",
    },
    Rule {
        pattern: r"\bsk-[A-Za-z0-9_-]{16,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bgithub_pat_[A-Za-z0-9_]{30,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bAKIA[0-9A-Z]{16}\b",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bAIza[0-9A-Za-z_-]{35}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r#"\b([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD)[A-Z0-9_]*)(\s*=\s*)("[^"]*"|'[^']*'|\S+)"#,
        replacement: "${1}${2}[redacted]",
    },
];

// A pattern that fails to compile is dropped here and caught by the `every_rule_compiles` test.
static COMPILED: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    RULES
        .iter()
        .filter_map(|rule| {
            Regex::new(rule.pattern)
                .ok()
                .map(|re| (re, rule.replacement))
        })
        .collect()
});

/// Agent text after redaction, at most `SNIPPET_MAX_BYTES` long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub struct Snippet(String);

pub fn redact(text: &str) -> String {
    apply_policy(text, &COMPILED, RULES.len())
}

impl Snippet {
    pub fn new(raw: &str) -> Self {
        Self(truncate(&redact(raw)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Snippet {
    fn from(raw: String) -> Self {
        Self::new(&raw)
    }
}

impl From<Snippet> for String {
    fn from(snippet: Snippet) -> Self {
        snippet.0
    }
}

fn apply_policy(raw: &str, compiled: &[(Regex, &'static str)], expected_rules: usize) -> String {
    if compiled.len() != expected_rules {
        return REDACTED.to_owned();
    }
    compiled
        .iter()
        .fold(raw.to_owned(), |acc, (re, replacement)| {
            re.replace_all(&acc, *replacement).into_owned()
        })
}

fn truncate(text: &str) -> String {
    if text.len() <= SNIPPET_MAX_BYTES {
        return text.to_owned();
    }
    let mut end = SNIPPET_MAX_BYTES.saturating_sub(ELLIPSIS.len_utf8());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push(ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn every_rule_compiles() {
        assert_eq!(COMPILED.len(), RULES.len());
    }

    #[test]
    fn missing_rule_redacts_everything() {
        let result = apply_policy(
            "API_KEY=abc plain text",
            COMPILED.get(..1).unwrap_or_default(),
            RULES.len(),
        );
        assert_eq!(result, REDACTED);
    }

    #[test]
    fn redacts_known_secret_shapes() {
        let cases = [
            ("API_KEY=abc123", "API_KEY=[redacted]"),
            (
                "export GITHUB_TOKEN='a b'",
                "export GITHUB_TOKEN=[redacted]",
            ),
            (
                "DB_PASSWORD = hunter2 rest",
                "DB_PASSWORD = [redacted] rest",
            ),
            ("Authorization: Bearer abc.def", "Authorization: [redacted]"),
            (
                "key sk-ant-api03-AAAAAAAAAAAAAAAAAAAA end",
                "key [redacted] end",
            ),
            ("ghp_abcdefghijklmnopqrstuvwxyz0123456789", "[redacted]"),
            ("id AKIAABCDEFGHIJKLMNOP", "id [redacted]"),
            ("AIzaSyA-abcdefghijklmnopqrstuvwxyz01234", "[redacted]"),
        ];
        for (raw, expected) in cases {
            assert_eq!(Snippet::new(raw).as_str(), expected, "input {raw:?}");
        }
    }

    #[test]
    fn redact_applies_rules_without_truncating() {
        assert_eq!(redact("API_KEY=x"), "API_KEY=[redacted]");
        let long = "a".repeat(5000);
        assert_eq!(redact(&long), long);
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let text = "cargo test --workspace; input_tokens: 2";
        assert_eq!(Snippet::new(text).as_str(), text);
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        let long = "é".repeat(2000);
        let snippet = Snippet::new(&long);
        assert!(snippet.as_str().len() <= SNIPPET_MAX_BYTES);
        assert!(snippet.as_str().ends_with('…'));
    }

    #[test]
    fn policy_is_idempotent() {
        let once = Snippet::new(&format!("API_KEY=x {}", "a".repeat(5000)));
        let twice = Snippet::new(once.as_str());
        assert_eq!(once, twice);
    }

    #[test]
    fn deserialising_applies_the_policy() {
        let json = format!("\"TOKEN=abc {}\"", "b".repeat(4000));
        let snippet: Snippet = serde_json::from_str(&json).unwrap();
        assert!(snippet.as_str().starts_with("TOKEN=[redacted]"));
        assert!(snippet.as_str().len() <= SNIPPET_MAX_BYTES);
    }
}
