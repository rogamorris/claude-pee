//! Argument parsing: extract claude-pee's own flags (`-p` and
//! `--output-format`) and forward everything else verbatim to the child.

use std::ffi::OsString;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    StreamJson,
    Json,
}

impl FromStr for OutputFormat {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "stream-json" => Ok(Self::StreamJson),
            "json" => Ok(Self::Json),
            other => Err(ParseError::InvalidOutputFormat(other.to_owned())),
        }
    }
}

#[derive(Debug)]
pub enum ParseError {
    MissingValue(&'static str),
    InvalidOutputFormat(String),
    InvalidPositiveInteger(&'static str, String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
            Self::InvalidOutputFormat(v) => write!(
                f,
                "invalid --output-format value {v:?}; expected one of: text, stream-json, json"
            ),
            Self::InvalidPositiveInteger(flag, value) => {
                write!(f, "invalid positive integer for {flag}: {value:?}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub struct ParsedArgs {
    pub inject: Option<String>,
    pub forwarded: Vec<OsString>,
    pub output_format: OutputFormat,
    pub capabilities: bool,
    pub run_id: Option<String>,
    pub lifecycle_path: Option<OsString>,
    pub inference_seconds: Option<u64>,
    pub normal_cleanup_seconds: u64,
    pub forced_cleanup_seconds: u64,
}

fn positive_integer(value: OsString, flag: &'static str) -> Result<u64, ParseError> {
    let text = value
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().into_owned());
    text.parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ParseError::InvalidPositiveInteger(flag, text))
}

pub fn parse<I: IntoIterator<Item = OsString>>(argv: I) -> Result<ParsedArgs, ParseError> {
    let mut inject: Option<String> = None;
    let mut forwarded: Vec<OsString> = Vec::new();
    let mut output_format = OutputFormat::Text;
    let mut capabilities = false;
    let mut run_id = None;
    let mut lifecycle_path = None;
    let mut inference_seconds = None;
    let mut normal_cleanup_seconds = 10;
    let mut forced_cleanup_seconds = 5;
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("-p") => {
                inject = iter.next().map(|v| v.to_string_lossy().into_owned());
            }
            Some(s) if let Some(rest) = s.strip_prefix("-p=") => {
                inject = Some(rest.to_owned());
            }
            Some("--output-format") => {
                let value = iter
                    .next()
                    .ok_or(ParseError::MissingValue("--output-format"))?;
                output_format = value.to_string_lossy().parse()?;
            }
            Some(s) if let Some(rest) = s.strip_prefix("--output-format=") => {
                output_format = rest.parse()?;
            }
            Some("--second-opinion-capabilities") => capabilities = true,
            Some("--second-opinion-run-id") => {
                run_id = Some(
                    iter.next()
                        .ok_or(ParseError::MissingValue("--second-opinion-run-id"))?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            Some("--second-opinion-lifecycle") => {
                lifecycle_path = Some(
                    iter.next()
                        .ok_or(ParseError::MissingValue("--second-opinion-lifecycle"))?,
                );
            }
            Some("--second-opinion-inference-seconds") => {
                inference_seconds = Some(positive_integer(
                    iter.next().ok_or(ParseError::MissingValue(
                        "--second-opinion-inference-seconds",
                    ))?,
                    "--second-opinion-inference-seconds",
                )?);
            }
            Some("--second-opinion-normal-cleanup-seconds") => {
                normal_cleanup_seconds = positive_integer(
                    iter.next().ok_or(ParseError::MissingValue(
                        "--second-opinion-normal-cleanup-seconds",
                    ))?,
                    "--second-opinion-normal-cleanup-seconds",
                )?;
            }
            Some("--second-opinion-forced-cleanup-seconds") => {
                forced_cleanup_seconds = positive_integer(
                    iter.next().ok_or(ParseError::MissingValue(
                        "--second-opinion-forced-cleanup-seconds",
                    ))?,
                    "--second-opinion-forced-cleanup-seconds",
                )?;
            }
            _ => forwarded.push(arg),
        }
    }
    Ok(ParsedArgs {
        inject,
        forwarded,
        output_format,
        capabilities,
        run_id,
        lifecycle_path,
        inference_seconds,
        normal_cleanup_seconds,
        forced_cleanup_seconds,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{OutputFormat, ParseError, ParsedArgs, parse};
    use std::ffi::OsString;

    fn osv(v: &[&str]) -> Vec<OsString> {
        v.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn dash_p_space_form() {
        let ParsedArgs {
            inject, forwarded, ..
        } = parse(osv(&["-p", "hello world", "file.txt"])).unwrap();
        assert_eq!(inject.as_deref(), Some("hello world"));
        assert_eq!(forwarded, osv(&["file.txt"]));
    }

    #[test]
    fn dash_p_equals_form() {
        let ParsedArgs {
            inject, forwarded, ..
        } = parse(osv(&["-p=hello world", "file.txt"])).unwrap();
        assert_eq!(inject.as_deref(), Some("hello world"));
        assert_eq!(forwarded, osv(&["file.txt"]));
    }

    #[test]
    fn forwards_everything_else() {
        let ParsedArgs {
            inject, forwarded, ..
        } = parse(osv(&["--version", "-T", "4", "file.txt"])).unwrap();
        assert!(inject.is_none());
        assert_eq!(forwarded, osv(&["--version", "-T", "4", "file.txt"]));
    }

    #[test]
    fn forwards_permission_mode_verbatim() {
        let ParsedArgs { forwarded, .. } =
            parse(osv(&["--permission-mode", "plan", "file.txt"])).unwrap();
        assert_eq!(forwarded, osv(&["--permission-mode", "plan", "file.txt"]));
    }

    #[test]
    fn last_dash_p_wins() {
        let ParsedArgs {
            inject, forwarded, ..
        } = parse(osv(&["-p", "first", "-p=second"])).unwrap();
        assert_eq!(inject.as_deref(), Some("second"));
        assert!(forwarded.is_empty());
    }

    #[test]
    fn dash_p_without_value_is_noop() {
        let ParsedArgs {
            inject, forwarded, ..
        } = parse(osv(&["-p"])).unwrap();
        assert!(inject.is_none());
        assert!(forwarded.is_empty());
    }

    #[test]
    fn output_format_default_is_text() {
        let ParsedArgs { output_format, .. } = parse(osv(&["file.txt"])).unwrap();
        assert_eq!(output_format, OutputFormat::Text);
    }

    #[test]
    fn output_format_space_form() {
        let ParsedArgs {
            output_format,
            forwarded,
            ..
        } = parse(osv(&["--output-format", "stream-json", "file.txt"])).unwrap();
        assert_eq!(output_format, OutputFormat::StreamJson);
        assert_eq!(forwarded, osv(&["file.txt"]));
    }

    #[test]
    fn output_format_equals_form() {
        let ParsedArgs {
            output_format,
            forwarded,
            ..
        } = parse(osv(&["--output-format=json", "file.txt"])).unwrap();
        assert_eq!(output_format, OutputFormat::Json);
        assert_eq!(forwarded, osv(&["file.txt"]));
    }

    #[test]
    fn output_format_text_accepted() {
        let ParsedArgs { output_format, .. } = parse(osv(&["--output-format", "text"])).unwrap();
        assert_eq!(output_format, OutputFormat::Text);
    }

    #[test]
    fn output_format_invalid_value_errors() {
        let err = parse(osv(&["--output-format", "yaml"])).unwrap_err();
        assert!(matches!(err, ParseError::InvalidOutputFormat(ref v) if v == "yaml"));
    }

    #[test]
    fn output_format_missing_value_errors() {
        let err = parse(osv(&["--output-format"])).unwrap_err();
        assert!(matches!(err, ParseError::MissingValue("--output-format")));
    }

    #[test]
    fn second_opinion_flags_are_owned_and_not_forwarded() {
        let parsed = parse(osv(&[
            "--second-opinion-run-id",
            "run-123",
            "--second-opinion-lifecycle",
            "/tmp/lifecycle.jsonl",
            "--second-opinion-inference-seconds",
            "300",
            "--second-opinion-normal-cleanup-seconds",
            "10",
            "--second-opinion-forced-cleanup-seconds",
            "5",
            "--model",
            "claude-fable-5",
        ]))
        .unwrap();
        assert_eq!(parsed.run_id.as_deref(), Some("run-123"));
        assert_eq!(parsed.inference_seconds, Some(300));
        assert_eq!(parsed.normal_cleanup_seconds, 10);
        assert_eq!(parsed.forced_cleanup_seconds, 5);
        assert_eq!(parsed.forwarded, osv(&["--model", "claude-fable-5"]));
    }

    #[test]
    fn capabilities_flag_is_owned() {
        let parsed = parse(osv(&["--second-opinion-capabilities"])).unwrap();
        assert!(parsed.capabilities);
        assert!(parsed.forwarded.is_empty());
    }
}
