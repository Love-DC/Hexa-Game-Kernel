use std::collections::BTreeSet;

use save_manager::compat::{
    CompatReasonCode, CompatibilityReport, CompatibilityVerdict, PolicyFlag,
};
use save_manager::diagnostics::{DiagnosticOutcome, SAVE_DIAGNOSTIC_SCHEMA, SaveDiagnosticReport};
use save_manager::loader::LoadError;
use save_manager::writer::{SystemManifestError, SystemManifestErrorCode};

fn validate_schema(report: &SaveDiagnosticReport) {
    let schema: serde_json::Value = serde_json::from_str(SAVE_DIAGNOSTIC_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let value = serde_json::to_value(report).unwrap();
    assert!(validator.is_valid(&value));
}

#[test]
fn compatibility_diagnostic_carries_the_user_decision_fields() {
    let compat = CompatibilityReport {
        verdict: CompatibilityVerdict::Blocked,
        reason_code: CompatReasonCode::ModMissing,
        flags: BTreeSet::from([PolicyFlag::MissingDependency]),
        affected_mods: vec!["org.example.economy".into()],
        save_version: Some("1.0".into()),
        current_version: Some("2.0".into()),
        failed_condition: "required Provider missing".into(),
        available_actions: vec!["Install Provider".into(), "Cancel".into()],
        needs_backup: true,
        may_lose_data: false,
        may_auto_continue: false,
    };
    let report = SaveDiagnosticReport::from_compatibility(&compat, Some("save-1".into()));
    assert_eq!(report.outcome, DiagnosticOutcome::Blocked);
    assert_eq!(report.code, "save.compat.mod_missing");
    assert_eq!(report.affected_mods, vec!["org.example.economy"]);
    assert!(report.needs_backup);
    validate_schema(&report);
}

#[test]
fn corruption_diagnostic_is_not_confused_with_compatibility_blocking() {
    let failure = SystemManifestError::new(
        SystemManifestErrorCode::Missing,
        "/metadata/session-data",
        "system manifest missing",
    );
    let report = SaveDiagnosticReport::from_load_error(
        &LoadError::SystemManifest(failure),
        Some("save-2".into()),
    );
    assert_eq!(report.outcome, DiagnosticOutcome::Corrupted);
    assert_eq!(report.code, "save.system_manifest_missing");
    assert!(!report.may_auto_continue);
    validate_schema(&report);
}

#[test]
fn diagnostic_json_is_stable_and_closed_to_unknown_fields() {
    let report = SaveDiagnosticReport {
        report_version: 1,
        outcome: DiagnosticOutcome::Failed,
        code: "save.migration.runner_failed".into(),
        save_id: Some("save-3".into()),
        affected_mods: vec![],
        current_version: None,
        recorded_version: None,
        failed_condition: "runner stopped".into(),
        available_actions: vec!["Restore backup".into()],
        needs_backup: true,
        may_lose_data: false,
        may_auto_continue: false,
    };
    assert_eq!(report.to_bytes(), report.to_bytes());
    let decoded: SaveDiagnosticReport = serde_json::from_slice(&report.to_bytes()).unwrap();
    assert_eq!(decoded, report);
    let mut value = serde_json::to_value(report).unwrap();
    value["surprise"] = serde_json::json!(true);
    assert!(serde_json::from_value::<SaveDiagnosticReport>(value).is_err());
}
