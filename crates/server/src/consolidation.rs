//! Process-local ownership of consolidation jobs shared by HTTP and the scheduler.
use chrono::{DateTime, Utc};
use merkur_core::ConsolidationReport;
use serde::Serialize;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedMutexGuard},
    task::JoinHandle,
};

#[derive(Clone, Serialize)]
pub struct ConsolidationStatus {
    pub state: &'static str,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub processed: usize,
    pub errors: usize,
}

pub struct ConsolidationControl {
    gate: Arc<AsyncMutex<()>>,
    status: Mutex<ConsolidationStatus>,
}

impl Default for ConsolidationControl {
    fn default() -> Self {
        Self {
            gate: Arc::new(AsyncMutex::new(())),
            status: Mutex::new(ConsolidationStatus {
                state: "idle",
                started_at: None,
                finished_at: None,
                processed: 0,
                errors: 0,
            }),
        }
    }
}

impl ConsolidationControl {
    pub fn snapshot(&self) -> ConsolidationStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn wait_idle(&self) {
        drop(self.gate.lock().await);
    }

    /// Dropping the returned handle detaches the job instead of cancelling it.
    pub fn start(
        self: &Arc<Self>,
        work: impl Future<Output = ConsolidationReport> + Send + 'static,
    ) -> Option<JoinHandle<ConsolidationReport>> {
        let permit = self.gate.clone().try_lock_owned().ok()?;
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = ConsolidationStatus {
            state: "running",
            started_at: Some(Utc::now()),
            finished_at: None,
            processed: 0,
            errors: 0,
        };
        let guard = RunGuard {
            control: self.clone(),
            _permit: permit,
            completed: false,
        };
        Some(tokio::spawn(async move {
            let mut guard = guard;
            let report = work.await;
            {
                let mut status = guard
                    .control
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                status.state = if report.errors == 0 {
                    "succeeded"
                } else {
                    "failed"
                };
                status.finished_at = Some(Utc::now());
                status.processed = report.memories_processed;
                status.errors = report.errors;
            }
            guard.completed = true;
            report
        }))
    }
}

struct RunGuard {
    control: Arc<ConsolidationControl>,
    _permit: OwnedMutexGuard<()>,
    completed: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.completed {
            let mut status = self
                .control
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status.state = "failed";
            status.finished_at = Some(Utc::now());
            status.errors = status.errors.max(1);
        }
    }
}
