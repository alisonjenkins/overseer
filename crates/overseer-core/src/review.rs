use serde::{Deserialize, Serialize};

use crate::CoreError;

/// The verdict returned by an automated reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Changes,
    Escalate,
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Changes => "CHANGES",
            Self::Escalate => "ESCALATE",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Verdict {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PASS" => Ok(Self::Pass),
            "CHANGES" => Ok(Self::Changes),
            "ESCALATE" => Ok(Self::Escalate),
            _ => Err(CoreError::UnknownVerdict {
                value: s.to_owned(),
            }),
        }
    }
}

/// Error returned when parsing a reviewer's response fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReviewParseError {
    #[error(
        "unparseable review verdict: first non-empty line must be VERDICT: PASS, VERDICT: CHANGES or VERDICT: ESCALATE"
    )]
    Unparseable,
}

/// A parsed review response containing the verdict and the reviewer's feedback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReview {
    pub verdict: Verdict,
    pub feedback: String,
}

/// Parse a reviewer's output. The FIRST non-empty line must be exactly
/// `VERDICT: PASS`, `VERDICT: CHANGES`, or `VERDICT: ESCALATE` (case-sensitive,
/// trailing whitespace allowed). Any other content is unparseable.
/// The remainder of the text is the reviewer's feedback.
pub fn parse_review(text: &str) -> Result<ParsedReview, ReviewParseError> {
    let mut lines = text.lines();
    let mut verdict = None;

    for line in lines.by_ref() {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed_end = line.trim_end();
        verdict = match trimmed_end {
            "VERDICT: PASS" => Some(Verdict::Pass),
            "VERDICT: CHANGES" => Some(Verdict::Changes),
            "VERDICT: ESCALATE" => Some(Verdict::Escalate),
            _ => return Err(ReviewParseError::Unparseable),
        };
        break;
    }

    let Some(verdict) = verdict else {
        return Err(ReviewParseError::Unparseable);
    };

    let remaining: Vec<&str> = lines.collect();
    let feedback = remaining.join("\n").trim().to_owned();

    Ok(ParsedReview { verdict, feedback })
}

/// Alias for `parse_review`.
pub fn parse_verdict(text: &str) -> Result<ParsedReview, ReviewParseError> {
    parse_review(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_verdict_pass_without_feedback() {
        let text = "VERDICT: PASS";
        let parsed = parse_review(text).unwrap();
        assert_eq!(parsed.verdict, Verdict::Pass);
        assert_eq!(parsed.feedback, "");
    }

    #[test]
    fn parses_verdict_pass_with_trailing_whitespace_and_leading_newlines() {
        let text = "  \n\r\nVERDICT: PASS   \t\n\nAll looks clean and solid.";
        let parsed = parse_review(text).unwrap();
        assert_eq!(parsed.verdict, Verdict::Pass);
        assert_eq!(parsed.feedback, "All looks clean and solid.");
    }

    #[test]
    fn parses_verdict_changes_with_bullet_list() {
        let text = "\
VERDICT: CHANGES

- Missing bounds check in tokenizer.rs
- Add unit test for empty input
";
        let parsed = parse_review(text).unwrap();
        assert_eq!(parsed.verdict, Verdict::Changes);
        assert_eq!(
            parsed.feedback,
            "- Missing bounds check in tokenizer.rs\n- Add unit test for empty input"
        );
    }

    #[test]
    fn parses_verdict_escalate_with_reasons() {
        let text = "\
VERDICT: ESCALATE

This change alters the authentication protocol and needs human sign-off.
";
        let parsed = parse_review(text).unwrap();
        assert_eq!(parsed.verdict, Verdict::Escalate);
        assert_eq!(
            parsed.feedback,
            "This change alters the authentication protocol and needs human sign-off."
        );
    }

    #[test]
    fn empty_text_is_unparseable() {
        assert_eq!(parse_review("").unwrap_err(), ReviewParseError::Unparseable);
        assert_eq!(
            parse_review("   \n\t\n  ").unwrap_err(),
            ReviewParseError::Unparseable
        );
    }

    #[test]
    fn leading_whitespace_on_verdict_line_is_unparseable() {
        let text = "  VERDICT: PASS\nlooks good";
        assert_eq!(
            parse_review(text).unwrap_err(),
            ReviewParseError::Unparseable
        );
    }

    #[test]
    fn case_sensitive_keyword_required() {
        assert_eq!(
            parse_review("verdict: pass").unwrap_err(),
            ReviewParseError::Unparseable
        );
        assert_eq!(
            parse_review("Verdict: Pass").unwrap_err(),
            ReviewParseError::Unparseable
        );
        assert_eq!(
            parse_review("VERDICT: pass").unwrap_err(),
            ReviewParseError::Unparseable
        );
    }

    #[test]
    fn extra_tokens_on_verdict_line_is_unparseable() {
        assert_eq!(
            parse_review("VERDICT: PASS - LGTM").unwrap_err(),
            ReviewParseError::Unparseable
        );
        assert_eq!(
            parse_review("VERDICT: CHANGES REQUIRED").unwrap_err(),
            ReviewParseError::Unparseable
        );
    }

    #[test]
    fn text_before_verdict_line_is_unparseable() {
        let text = "Here is my review:\nVERDICT: PASS";
        assert_eq!(
            parse_review(text).unwrap_err(),
            ReviewParseError::Unparseable
        );
    }

    #[test]
    fn verdict_round_trips_through_str_and_serde() {
        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct TestWrapper {
            verdict: Verdict,
        }

        for (verdict, expected) in [
            (Verdict::Pass, "PASS"),
            (Verdict::Changes, "CHANGES"),
            (Verdict::Escalate, "ESCALATE"),
        ] {
            assert_eq!(verdict.as_str(), expected);
            assert_eq!(verdict.to_string(), expected);
            assert_eq!(expected.parse::<Verdict>().unwrap(), verdict);

            let wrapper = TestWrapper { verdict };
            let encoded = toml::to_string(&wrapper).unwrap();
            assert_eq!(encoded.trim(), format!("verdict = \"{expected}\""));
            let back: TestWrapper = toml::from_str(&encoded).unwrap();
            assert_eq!(back, wrapper);
        }

        assert!("UNKNOWN".parse::<Verdict>().is_err());
    }
}
