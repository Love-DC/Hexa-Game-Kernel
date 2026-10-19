//! `SAVE-011`: machine-readable save diagnostics derived from existing results.

use serde::{Deserialize, Serialize};

use crate::backup::BackupError;
use crate::compat::{CompatibilityReport, CompatibilityVerdict};
use crate::loader::LoadError;
use crate::mod_changes::{ModChangeDecision, ModChangeDisposition};
use crate::references::ReferenceValidationError;
use crate::transaction::TransactionError;

pub const SAVE_DIAGNOSTIC_VERSION: u32 = 1;
pub const SAVE_DIAGNOSTIC_SCHEMA: &str =
    include_str!("../../../schemas/save-diagnostic-v1.schema.json");

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOutcome {
    Compatible,
    CompatibleWithMigration,
    Blocked,
    Corrupted,
    Failed,
}

/// Versioned report fields required by Save §13. Stable codes are protocol;
/// `failed_condition` is a display/developer explanation only.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SaveDiagnosticReport {
    pub report_version: u32,
    pub outcome: DiagnosticOutcome,
    pub code: String,
    pub save_id: Option<String>,
    pub affected_mods: Vec<String>,
    pub current_version: Option<String>,
    pub recorded_version: Option<String>,
    pub failed_condition: String,
    pub available_actions: Vec<String>,
    pub needs_backup: bool,
    pub may_lose_data: bool,
    pub may_auto_continue: bool,
}

impl SaveDiagnosticReport {
    pub fn from_mod_change(decision: &ModChangeDecision, save_id: Option<String>) -> Self {
        let outcome = match decision.disposition {
            ModChangeDisposition::AllowedDirectly => DiagnosticOutcome::Compatible,
            ModChangeDisposition::RequiresMigration => DiagnosticOutcome::CompatibleWithMigration,
            ModChangeDisposition::Blocked => DiagnosticOutcome::Blocked,
        };
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome,
            code: decision.code.to_owned(),
            save_id,
            affected_mods: decision.affected_mods.clone(),
            current_version: None,
            recorded_version: None,
            failed_condition: decision.failed_condition.clone(),
            available_actions: decision.available_actions.clone(),
            needs_backup: decision.needs_backup,
            may_lose_data: decision.may_lose_data,
            may_auto_continue: decision.may_auto_continue,
        }
    }

    pub fn from_compatibility(report: &CompatibilityReport, save_id: Option<String>) -> Self {
        let outcome = match report.verdict {
            CompatibilityVerdict::Compatible => DiagnosticOutcome::Compatible,
            CompatibilityVerdict::CompatibleWithMigration => {
                DiagnosticOutcome::CompatibleWithMigration
            }
            CompatibilityVerdict::Blocked => DiagnosticOutcome::Blocked,
            CompatibilityVerdict::Corrupted => DiagnosticOutcome::Corrupted,
        };
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome,
            code: report.reason_code.as_str().to_owned(),
            save_id,
            affected_mods: report.affected_mods.clone(),
            current_version: report.current_version.clone(),
            recorded_version: report.save_version.clone(),
            failed_condition: report.failed_condition.clone(),
            available_actions: report.available_actions.clone(),
            needs_backup: report.needs_backup,
            may_lose_data: report.may_lose_data,
            may_auto_continue: report.may_auto_continue,
        }
    }

    pub fn from_load_error(error: &LoadError, save_id: Option<String>) -> Self {
        let (code, details, outcome) = match error {
            LoadError::Io { stage, .. } => (
                "save.load.io",
                format!("load failed during {}", stage.as_str()),
                DiagnosticOutcome::Failed,
            ),
            LoadError::Layout(failure) => (
                failure.code_str(),
                failure.to_string(),
                DiagnosticOutcome::Corrupted,
            ),
            LoadError::Manifest(failure) => (
                failure.code_str(),
                failure.to_string(),
                DiagnosticOutcome::Corrupted,
            ),
            LoadError::Integrity(failure) => (
                failure.code_str(),
                failure.to_string(),
                DiagnosticOutcome::Corrupted,
            ),
            LoadError::Corrupted(report) => (
                "save.integrity.mismatch",
                format!(
                    "recorded root {} differs from recomputed root {}",
                    report.manifest_root, report.computed_root
                ),
                DiagnosticOutcome::Corrupted,
            ),
            LoadError::SystemManifest(failure) => (
                failure.code_str(),
                failure.to_string(),
                DiagnosticOutcome::Corrupted,
            ),
        };
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome,
            code: code.to_owned(),
            save_id,
            affected_mods: Vec::new(),
            current_version: None,
            recorded_version: None,
            failed_condition: details,
            available_actions: vec!["Restore a verified backup".into(), "Cancel loading".into()],
            needs_backup: false,
            may_lose_data: false,
            may_auto_continue: false,
        }
    }

    pub fn from_reference_error(error: &ReferenceValidationError, save_id: Option<String>) -> Self {
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome: DiagnosticOutcome::Blocked,
            code: error.code.to_owned(),
            save_id,
            affected_mods: Vec::new(),
            current_version: None,
            recorded_version: None,
            failed_condition: error.message.clone(),
            available_actions: vec![
                "Install or restore the required Provider".into(),
                "Cancel loading".into(),
            ],
            needs_backup: true,
            may_lose_data: false,
            may_auto_continue: false,
        }
    }

    pub fn from_transaction_error(error: &TransactionError, save_id: Option<String>) -> Self {
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome: DiagnosticOutcome::Failed,
            code: error.code().to_owned(),
            save_id,
            affected_mods: Vec::new(),
            current_version: None,
            recorded_version: None,
            failed_condition: error.to_string(),
            available_actions: vec![
                "Restore the verified recovery copy".into(),
                "Cancel migration".into(),
            ],
            needs_backup: true,
            may_lose_data: false,
            may_auto_continue: false,
        }
    }

    pub fn from_backup_error(error: &BackupError, save_id: Option<String>) -> Self {
        let corrupted =
            matches!(error, BackupError::Load(_)) || matches!(error, BackupError::Metadata { .. });
        Self {
            report_version: SAVE_DIAGNOSTIC_VERSION,
            outcome: if corrupted {
                DiagnosticOutcome::Corrupted
            } else {
                DiagnosticOutcome::Failed
            },
            code: error.code().to_owned(),
            save_id,
            affected_mods: Vec::new(),
            current_version: None,
            recorded_version: None,
            failed_condition: error.to_string(),
            available_actions: vec![
                "Keep the current save unchanged".into(),
                "Choose another verified backup".into(),
                "Cancel".into(),
            ],
            needs_backup: false,
            may_lose_data: false,
            may_auto_continue: false,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("diagnostic report serializes");
        bytes.push(b'\n');
        bytes
    }
}
