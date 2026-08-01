//! Content-free lifecycle protocol for coordinated one-shot reviews.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Launched,
    PromptSubmitted,
    OutputObserved,
    OutputComplete,
    CleanupStarted,
    Terminated,
}

impl Phase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Launched => "launched",
            Self::PromptSubmitted => "prompt_submitted",
            Self::OutputObserved => "output_observed",
            Self::OutputComplete => "output_complete",
            Self::CleanupStarted => "cleanup_started",
            Self::Terminated => "terminated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    CompletedClean,
    CompletedForcedCleanup,
    FailedIncomplete,
    CleanupFailed,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CompletedClean => "completed_clean",
            Self::CompletedForcedCleanup => "completed_forced_cleanup",
            Self::FailedIncomplete => "failed_incomplete",
            Self::CleanupFailed => "cleanup_failed",
        }
    }
}

pub struct Emitter {
    run_id: String,
    start: Instant,
    file: File,
}

impl Emitter {
    pub fn open(path: &Path, run_id: String, start: Instant) -> io::Result<Self> {
        let file = OpenOptions::new().create_new(true).write(true).open(path)?;
        Ok(Self {
            run_id,
            start,
            file,
        })
    }

    pub fn emit(&mut self, phase: Phase, outcome: Option<Outcome>, reason: &str) -> io::Result<()> {
        let elapsed_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut record = serde_json::Map::new();
        record.insert("schema_version".to_owned(), SCHEMA_VERSION.into());
        record.insert("run_id".to_owned(), self.run_id.clone().into());
        record.insert("phase".to_owned(), phase.as_str().into());
        record.insert("elapsed_ms".to_owned(), elapsed_ms.into());
        if let Some(outcome) = outcome {
            record.insert("outcome".to_owned(), outcome.as_str().into());
        }
        if !reason.is_empty() {
            record.insert("reason".to_owned(), reason.into());
        }
        writeln!(self.file, "{}", serde_json::Value::Object(record))?;
        self.file.flush()
    }
}

pub const fn within_deadline(elapsed: Duration, deadline: Duration) -> bool {
    elapsed.as_nanos() <= deadline.as_nanos()
}

#[cfg(test)]
mod tests {
    use super::within_deadline;
    use std::time::Duration;

    #[test]
    fn deadline_equality_counts_as_complete() {
        let deadline = Duration::from_mins(5);
        assert!(within_deadline(Duration::from_millis(299_999), deadline));
        assert!(within_deadline(deadline, deadline));
        assert!(!within_deadline(Duration::from_millis(300_001), deadline));
    }
}
