use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use overseer_core::{RunOutcome, RunRequest, Usage};

use crate::{Backend, BackendError};

/// A backend that replays scripted outcomes and records every request it
/// receives. Intended for tests of the orchestrator and CLI.
#[derive(Debug, Default)]
pub struct MockBackend {
    /// `Ok` is a classified outcome; `Err` is a scripted transport-level error
    /// carrying its message.
    script: Mutex<VecDeque<Result<RunOutcome, String>>>,
    requests: Mutex<Vec<RunRequest>>,
}

impl MockBackend {
    /// Replays `script` in order, one entry per call.
    pub fn new(script: Vec<Result<RunOutcome, String>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A single successful answer with zero usage.
    pub fn answering(output: &str) -> Self {
        Self::new(vec![Ok(RunOutcome {
            output: output.to_owned(),
            usage: Usage::default(),
            failure: None,
        })])
    }

    /// Every request the backend has been asked to run, in call order.
    pub fn requests(&self) -> Vec<RunRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Backend for MockBackend {
    async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req.clone());
        let next = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(message)) => Err(BackendError::Scripted { message }),
            None => Err(BackendError::MockExhausted),
        }
    }
}
