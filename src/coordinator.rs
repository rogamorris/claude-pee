//! Ordered lifecycle-v1 state transitions for coordinated reviews.

use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::lifecycle::{Emitter, Phase};
use crate::transcript::{self, Observation};

pub struct Coordinator {
    prompt_emitted: bool,
    output_emitted: bool,
    review: Option<String>,
    cleanup_started: Option<Instant>,
}

impl Coordinator {
    pub const fn new() -> Self {
        Self {
            prompt_emitted: false,
            output_emitted: false,
            review: None,
            cleanup_started: None,
        }
    }

    pub const fn review(&self) -> Option<&String> {
        self.review.as_ref()
    }

    pub const fn take_review(&mut self) -> Option<String> {
        self.review.take()
    }

    pub const fn cleanup_started(&self) -> Option<Instant> {
        self.cleanup_started
    }

    pub fn emit_prompt_if_ready(
        &mut self,
        prompt_rx: &Receiver<()>,
        lifecycle: &mut Emitter,
    ) -> io::Result<()> {
        if self.cleanup_started.is_none() && !self.prompt_emitted && prompt_rx.try_recv().is_ok() {
            lifecycle.emit(Phase::PromptSubmitted, None, "")?;
            self.prompt_emitted = true;
        }
        Ok(())
    }

    pub fn record_observation(
        &mut self,
        observation: Observation,
        prompt_rx: &Receiver<()>,
        lifecycle: &mut Emitter,
        inference: Duration,
    ) -> io::Result<()> {
        if self.cleanup_started.is_some() {
            return Ok(());
        }
        if !self.prompt_emitted {
            match prompt_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => {
                    lifecycle.emit(Phase::PromptSubmitted, None, "")?;
                    self.prompt_emitted = true;
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        if !self.output_emitted {
            lifecycle.emit(Phase::OutputObserved, None, "")?;
            self.output_emitted = true;
        }
        if transcript::completed_within(&observation, inference) && self.review.is_none() {
            lifecycle.emit(Phase::OutputComplete, None, "")?;
            self.review = Some(observation.text);
        }
        Ok(())
    }

    pub fn begin_cleanup(&mut self, lifecycle: &mut Emitter, reason: &str) -> io::Result<()> {
        if self.cleanup_started.is_none() {
            lifecycle.emit(Phase::CleanupStarted, None, reason)?;
            self.cleanup_started = Some(Instant::now());
        }
        Ok(())
    }
}
