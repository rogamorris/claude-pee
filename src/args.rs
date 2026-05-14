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
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
            Self::InvalidOutputFormat(v) => write!(
                f,
                "invalid --output-format value {v:?}; expected one of: text, stream-json, json"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub struct ParsedArgs {
    pub inject: Option<String>,
    pub forwarded: Vec<OsString>,
    pub output_format: OutputFormat,
}

pub fn parse<I: IntoIterator<Item = OsString>>(argv: I) -> Result<ParsedArgs, ParseError> {
    let mut inject: Option<String> = None;
    let mut forwarded: Vec<OsString> = Vec::new();
    let mut output_format = OutputFormat::Text;
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
            _ => forwarded.push(arg),
        }
    }
    Ok(ParsedArgs {
        inject,
        forwarded,
        output_format,
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
}
