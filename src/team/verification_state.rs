//! Completion verification state: the phase, iteration budget and evidence
//! recorded for an engineer's task while its completion is verified.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPhase {
    Executing,
    Verifying,
    Fixing,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    CommitsAhead,
    FilesChanged,
    CodeFilesChanged,
    TestsPassed,
    TestsFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub kind: EvidenceKind,
    pub detail: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationState {
    pub phase: VerificationPhase,
    pub iteration: u32,
    pub max_iterations: u32,
    pub last_test_output: Option<String>,
    pub last_test_passed: bool,
    pub evidence: Vec<VerificationEvidence>,
}

impl VerificationState {
    pub fn new(max_iterations: u32) -> Self {
        Self {
            phase: VerificationPhase::Executing,
            iteration: 0,
            max_iterations: max_iterations.max(1),
            last_test_output: None,
            last_test_passed: false,
            evidence: Vec::new(),
        }
    }

    pub fn transition(&mut self, phase: VerificationPhase) -> VerificationPhase {
        let previous = self.phase.clone();
        self.phase = phase;
        previous
    }

    pub fn begin_iteration(&mut self) {
        self.iteration = self.iteration.saturating_add(1);
    }

    pub fn record_evidence(&mut self, kind: EvidenceKind, detail: impl Into<String>) {
        self.evidence.push(VerificationEvidence {
            kind,
            detail: detail.into(),
            timestamp: chrono::Utc::now(),
        });
    }

    pub fn reached_max_iterations(&self) -> bool {
        self.iteration >= self.max_iterations
    }

    pub fn clear_evidence(&mut self) {
        self.evidence.clear();
    }
}
