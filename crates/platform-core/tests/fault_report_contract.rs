//! GK-P3-06 fault-report contract tests.
//!
//! These tests exercise the frozen rev2 design
//! (`planning/16-P3-06-故障报告契约设计提案.md`) and its Schema
//! (`schemas/mod-fault-report-v1.schema.json`). Coverage maps to the design's
//! §9.3 test plan (T-1 .. T-12):
//!
//! * T-1  all 14 categories reachable and Schema-valid
//! * T-2  `report_version` pinned by Schema; unknown fields rejected
//! * T-3  runtime fuel exhaustion -> `resource.fuel_exceeded`
//! * T-4  instantiation-phase memory limit -> `resource.memory_limit_exceeded`
//! * T-5  runtime memory OOB -> `resource.memory_out_of_bounds`
//! * T-6  guest trap subdivisions
//! * T-7  `message` does not affect code/disposition/authority
//! * T-8  §4 continue/stop/rebuild decision table
//! * T-9  §5 four-question authority table (never default to continue)
//! * T-10 `SystemError` -> `ModFaultReport` is one-way; Kernel has no reverse dep
//! * T-11 budget configurable with `config_version`; no perf claim
//!
//! O-6-1 / O-6-2: the `determinism` / `migration` categories exist as
//! placeholders only. No test fabricates a detection result for them; they are
//! covered solely as *code/decision table* entries.

use std::collections::BTreeSet;

use game_platform::{
    Disposition, FAULT_REPORT_VERSION, FaultCategory, FaultPhase, LimitKind, LoadedWasmMod,
    ModFailureCode, ModFaultReport, ModPackage, PlatformError, RecoveryAdvice, ResourceUse,
    SeverityGrade, create_wasm_engine, validate_fault_report_value,
};
use kernel_core::{ComponentValue, Kernel, KernelError, StepInput, Tick, World, WorldId};

fn project_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn grants() -> BTreeSet<String> {
    BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()])
}

/// Compiles a fixture and runs it in a fresh Kernel, returning the stable
/// `SystemError` produced by the first step.
fn run_fixture(fixture: &str) -> kernel_core::SystemError {
    let package = ModPackage::load(project_path(&format!("tests/fixtures/{fixture}"))).unwrap();
    let engine = create_wasm_engine().unwrap();
    let loaded = LoadedWasmMod::compile(&package, &grants(), &engine).unwrap();
    let mut world = World::new(WorldId(7));
    world
        .seed_resource("org.game/counter", ComponentValue::Integer(0))
        .unwrap();
    let mut kernel = Kernel::new(world).unwrap();
    kernel.register_system(Box::new(loaded.system())).unwrap();
    match kernel.step(StepInput::default()) {
        Err(KernelError::SystemFailed(error)) => error,
        other => panic!("expected a stable Mod failure for `{fixture}`, got {other:?}"),
    }
}

/// A representative report for every one of the 14 categories, built from the
/// frozen `code` -> decision table.
fn one_report_per_category() -> Vec<(&'static str, ModFaultReport)> {
    let codes = [
        // (stable code, category, phase, severity, disposition, retryable, advice)
        "platform.invalid_manifest",
        "platform.invalid_package",
        "platform.dependency_resolution",
        "platform.dependency.version_incompatible",
        "platform.permission_denied",
        "platform.mod.load_failed",
        "platform.mod.initialization_failed",
        "platform.api_catalog",
        "platform.mod.api.invocation_failed",
        "platform.mod.guest_trap.unreachable",
        "platform.mod.resource.fuel_exceeded",
        "platform.mod.determinism_violation",
        "platform.lifecycle.registration_rejected",
        "platform.mod.migration_failed",
    ];
    codes
        .into_iter()
        .map(|code| {
            // Several categories carry `fix_manifest` advice, which the frozen
            // Schema's cross-field rule pairs with a Mod identity.
            let report = ModFaultReport::new(code, format!("sample for {code}"))
                .with_mod_id("org.game.sample");
            (code, report)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// T-1: all 14 categories reachable + Schema-valid.
// ---------------------------------------------------------------------------

#[test]
fn all_fourteen_categories_are_reachable_and_schema_valid() {
    let reports = one_report_per_category();
    assert_eq!(reports.len(), 14);

    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for (code, report) in &reports {
        assert_eq!(report.code, *code);
        assert_eq!(report.report_version, FAULT_REPORT_VERSION);
        // Everything a machine needs is derived from `code`, not `message`.
        let value = serde_json::to_value(report).unwrap();
        validate_fault_report_value(&value)
            .unwrap_or_else(|error| panic!("report for `{code}` must be Schema-valid: {error}"));
        seen.insert(report.category.as_str());
    }
    // All 14 frozen categories are represented.
    let expected: BTreeSet<&'static str> = [
        FaultCategory::Manifest,
        FaultCategory::Package,
        FaultCategory::Dependency,
        FaultCategory::VersionConflict,
        FaultCategory::Permission,
        FaultCategory::Load,
        FaultCategory::Initialization,
        FaultCategory::ApiRegistration,
        FaultCategory::ApiInvocation,
        FaultCategory::WasmTrap,
        FaultCategory::ResourceLimit,
        FaultCategory::Determinism,
        FaultCategory::Lifecycle,
        FaultCategory::Migration,
    ]
    .into_iter()
    .map(FaultCategory::as_str)
    .collect();
    assert_eq!(seen, expected);
}

// ---------------------------------------------------------------------------
// T-2: report_version pinned; unknown fields rejected.
// ---------------------------------------------------------------------------

#[test]
fn report_version_is_pinned_by_schema() {
    let report = ModFaultReport::new("platform.invalid_manifest", "x");
    let mut value = serde_json::to_value(&report).unwrap();
    value["report_version"] = serde_json::json!(2);
    assert!(
        validate_fault_report_value(&value).is_err(),
        "report_version 2 must be rejected by the v1 schema"
    );
}

#[test]
fn unknown_fields_are_rejected_by_schema() {
    let report = ModFaultReport::new("platform.invalid_manifest", "x");
    let mut value = serde_json::to_value(&report).unwrap();
    value["unexpected_future_field"] = serde_json::json!("boom");
    let error = validate_fault_report_value(&value).unwrap_err();
    assert!(
        error.contains("unexpected_future_field") || error.contains("additional"),
        "unknown field must be rejected, got: {error}"
    );
}

#[test]
fn unknown_fields_are_rejected_by_serde() {
    // Deserialization mirrors the Schema: `deny_unknown_fields`.
    let json = serde_json::json!({
        "report_version": 1,
        "code": "platform.invalid_manifest",
        "category": "manifest",
        "phase": "manifest",
        "severity_grade": "l1_call",
        "disposition": "stop",
        "authoritative": false,
        "retryable": false,
        "recovery_advice": "fix_manifest",
        "message": "x",
        "totally_unknown": 1
    });
    assert!(serde_json::from_value::<ModFaultReport>(json).is_err());
}

// ---------------------------------------------------------------------------
// T-3: runtime fuel exhaustion.
// ---------------------------------------------------------------------------

#[test]
fn runtime_fuel_exhaustion_reports_resource_fuel_exceeded() {
    let error = run_fixture("fuel-loop");
    assert_eq!(error.code, "platform.mod.resource.fuel_exceeded");
    assert_ne!(error.code, ModFailureCode::ExecutionFailed.as_str());

    let report = ModFaultReport::from_system_error(&error);
    assert_eq!(report.category, FaultCategory::ResourceLimit);
    assert_eq!(report.phase, FaultPhase::Execution);
    assert_eq!(report.severity_grade, SeverityGrade::L2Task);
    assert_eq!(report.disposition, Disposition::Stop);
    assert!(report.retryable, "fuel exhaustion is recoverable by budget");
    assert_eq!(report.recovery_advice, RecoveryAdvice::RetryCall);
    validate_fault_report_value(&serde_json::to_value(&report).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// T-4: instantiation-phase memory limit.
// ---------------------------------------------------------------------------

#[test]
fn instantiation_memory_limit_reports_distinct_code_and_phase() {
    let package = ModPackage::load(project_path("tests/fixtures/memory-limit")).unwrap();
    let engine = create_wasm_engine().unwrap();
    let error = match LoadedWasmMod::compile(&package, &grants(), &engine) {
        Ok(_) => panic!("over-limit module must be rejected before registration"),
        Err(error) => error,
    };
    assert_eq!(
        error.fault_code(),
        "platform.mod.resource.memory_limit_exceeded"
    );

    let report = ModFaultReport::from_platform_error(&error);
    assert_eq!(report.code, "platform.mod.resource.memory_limit_exceeded");
    assert_eq!(report.category, FaultCategory::ResourceLimit);
    assert_eq!(report.phase, FaultPhase::Instantiation);
    assert_eq!(report.disposition, Disposition::Stop);
    assert!(!report.retryable);
    assert_eq!(report.recovery_advice, RecoveryAdvice::DisableMod);
    assert_eq!(report.mod_id.as_deref(), Some("org.game.memory-limit"));
    validate_fault_report_value(&serde_json::to_value(&report).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// T-5: runtime memory OOB (distinct from T-4).
// ---------------------------------------------------------------------------

#[test]
fn runtime_memory_oob_reports_distinct_code_and_phase() {
    let error = run_fixture("memory-oob");
    assert_eq!(error.code, "platform.mod.resource.memory_out_of_bounds");

    let report = ModFaultReport::from_system_error(&error);
    assert_eq!(report.category, FaultCategory::ResourceLimit);
    assert_eq!(report.phase, FaultPhase::Execution);
    assert_eq!(report.disposition, Disposition::Stop);
    assert!(!report.retryable);
    validate_fault_report_value(&serde_json::to_value(&report).unwrap()).unwrap();

    // The two memory paths must not collapse onto one code/phase.
    assert_ne!(report.code, "platform.mod.resource.memory_limit_exceeded");
    assert_ne!(report.phase, FaultPhase::Instantiation);
}

// ---------------------------------------------------------------------------
// T-6: guest trap subdivisions.
// ---------------------------------------------------------------------------

#[test]
fn guest_unreachable_reports_unreachable_code() {
    let error = run_fixture("guest-unreachable");
    assert_eq!(error.code, "platform.mod.guest_trap.unreachable");
    let report = ModFaultReport::from_system_error(&error);
    assert_eq!(report.category, FaultCategory::WasmTrap);
    assert_eq!(report.phase, FaultPhase::Execution);
    assert_eq!(report.disposition, Disposition::Stop);
    assert!(!report.retryable);
    validate_fault_report_value(&serde_json::to_value(&report).unwrap()).unwrap();
}

#[test]
fn guest_divide_by_zero_reports_divide_by_zero_code() {
    let error = run_fixture("guest-div-zero");
    assert_eq!(error.code, "platform.mod.guest_trap.integer_divide_by_zero");
    let report = ModFaultReport::from_system_error(&error);
    assert_eq!(report.category, FaultCategory::WasmTrap);
    assert_eq!(report.disposition, Disposition::Stop);
    validate_fault_report_value(&serde_json::to_value(&report).unwrap()).unwrap();
}

#[test]
fn interrupt_trap_is_the_only_retryable_guest_trap() {
    // The `Interrupt` trap's frozen code (frozen §3.2) is the one retryable
    // guest trap; it is exercised here through the report decision table. The
    // variant-level classification is covered by the in-crate unit test
    // `trap_variants_map_to_frozen_stable_codes`.
    let report = ModFaultReport::new("platform.mod.guest_trap.interrupted", "interrupt");
    assert_eq!(report.category, FaultCategory::WasmTrap);
    assert!(report.retryable);
    assert_eq!(report.recovery_advice, RecoveryAdvice::RetryCall);
    // Every other guest trap code is non-retryable.
    for code in [
        "platform.mod.guest_trap.unreachable",
        "platform.mod.guest_trap.integer_overflow",
        "platform.mod.guest_trap.integer_divide_by_zero",
        "platform.mod.guest_trap.invalid_conversion",
        "platform.mod.guest_trap.other",
        "platform.mod.guest_trap.unknown",
    ] {
        assert!(!ModFaultReport::new(code, "x").retryable, "`{code}`");
    }
}

// ---------------------------------------------------------------------------
// T-7: message text is not a protocol field.
// ---------------------------------------------------------------------------

#[test]
fn message_does_not_change_code_disposition_or_authority() {
    let a = ModFaultReport::new("platform.mod.resource.fuel_exceeded", "first wording");
    let b = ModFaultReport::new(
        "platform.mod.resource.fuel_exceeded",
        "completely different localized wording 燃料耗尽",
    );
    assert_ne!(a.message, b.message);
    // Everything machine-visible is identical.
    assert_eq!(a.code, b.code);
    assert_eq!(a.category, b.category);
    assert_eq!(a.phase, b.phase);
    assert_eq!(a.severity_grade, b.severity_grade);
    assert_eq!(a.disposition, b.disposition);
    assert_eq!(a.authoritative, b.authoritative);
    assert_eq!(a.retryable, b.retryable);
    assert_eq!(a.recovery_advice, b.recovery_advice);
}

// ---------------------------------------------------------------------------
// T-8: §4 continue/stop/rebuild decision table.
// ---------------------------------------------------------------------------

#[test]
fn disposal_decision_table_matches_frozen_section_4() {
    // (code, expected disposition, expected severity)
    let cases: &[(&str, Disposition, SeverityGrade)] = &[
        (
            "platform.invalid_manifest",
            Disposition::Stop,
            SeverityGrade::L1Call,
        ),
        (
            "platform.invalid_package",
            Disposition::Stop,
            SeverityGrade::L1Call,
        ),
        (
            "platform.dependency_resolution",
            Disposition::Stop,
            SeverityGrade::L1Call,
        ),
        (
            "platform.dependency.version_incompatible",
            Disposition::Stop,
            SeverityGrade::L1Call,
        ),
        (
            "platform.permission_denied",
            Disposition::Stop,
            SeverityGrade::L1Call,
        ),
        (
            "platform.mod.load_failed",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.initialization_failed",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.api_catalog",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.api.invocation_failed",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.guest_trap.unreachable",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.resource.fuel_exceeded",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.resource.memory_limit_exceeded",
            Disposition::Stop,
            SeverityGrade::L2Task,
        ),
        (
            "platform.mod.determinism_violation",
            Disposition::Rebuild,
            SeverityGrade::L4Runtime,
        ),
        (
            "platform.lifecycle.registration_rejected",
            Disposition::Stop,
            SeverityGrade::L3Mod,
        ),
        (
            "platform.mod.migration_failed",
            Disposition::Rebuild,
            SeverityGrade::L4Runtime,
        ),
    ];
    for (code, disposition, severity) in cases {
        let report = ModFaultReport::new(*code, "x");
        assert_eq!(report.disposition, *disposition, "disposition for `{code}`");
        assert_eq!(report.severity_grade, *severity, "severity for `{code}`");
    }
}

#[test]
fn disposition_invariants_hold_for_every_known_code() {
    // §4.2 invariants, checked over the whole frozen code table.
    let codes = [
        "platform.invalid_manifest",
        "platform.invalid_package",
        "platform.dependency_resolution",
        "platform.dependency.version_incompatible",
        "platform.permission_denied",
        "platform.permission_not_requested",
        "platform.mod.load_failed",
        "platform.mod.initialization_failed",
        "platform.api_catalog",
        "platform.mod.api.invocation_failed",
        "platform.mod.guest_trap.unreachable",
        "platform.mod.guest_trap.interrupted",
        "platform.mod.guest_trap.other",
        "platform.mod.resource.fuel_exceeded",
        "platform.mod.resource.memory_out_of_bounds",
        "platform.mod.resource.memory_limit_exceeded",
        "platform.mod.determinism_violation",
        "platform.lifecycle.registration_rejected",
        "platform.mod.migration_failed",
    ];
    for code in codes {
        let report = ModFaultReport::new(code, "x");
        // No category defaults to `continue` in this stage.
        assert_ne!(
            report.disposition,
            Disposition::Continue,
            "`{code}` must not continue"
        );
        // High severity => stop/rebuild and non-retryable.
        if matches!(
            report.severity_grade,
            SeverityGrade::L3Mod | SeverityGrade::L4Runtime
        ) {
            assert!(!report.retryable, "`{code}` must not be retryable");
        }
        // rebuild => advice is a rebuild/restore advice.
        if report.disposition == Disposition::Rebuild {
            assert!(matches!(
                report.recovery_advice,
                RecoveryAdvice::RebuildInstance | RecoveryAdvice::RestoreCheckpoint
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// T-9: §5 four-question authority table (never default to continue).
// ---------------------------------------------------------------------------

#[test]
fn uncertain_authority_is_conservatively_rebuilt() {
    // A code whose state cannot be proven consistent must be authoritative and
    // rebuild (§5 rows 2/4/5/8). Determinism/migration hit the rebuild branch.
    for code in [
        "platform.mod.determinism_violation",
        "platform.mod.migration_failed",
    ] {
        let report = ModFaultReport::new(code, "x");
        assert!(report.authoritative, "`{code}` must be authoritative");
        assert_eq!(report.disposition, Disposition::Rebuild);
    }
}

#[test]
fn provably_rolled_back_trap_is_not_authoritative_and_stops() {
    // §5.4: the fuel-loop trap is rolled back and provably consistent
    // (Q1=yes, Q2=yes, Q3=yes) => authoritative=false, disposition=stop.
    let error = run_fixture("fuel-loop");
    let report = ModFaultReport::from_system_error(&error);
    assert!(
        !report.authoritative,
        "a rolled-back trap is not authoritative"
    );
    assert_eq!(report.disposition, Disposition::Stop);
}

#[test]
fn unknown_code_falls_back_to_conservative_rebuild() {
    // §5 row 8: an unrecognised code must never default to continue.
    let report = ModFaultReport::new("platform.some.future.unknown_code", "x");
    assert!(report.authoritative);
    assert_eq!(report.disposition, Disposition::Rebuild);
    assert!(matches!(
        report.recovery_advice,
        RecoveryAdvice::RebuildInstance | RecoveryAdvice::RestoreCheckpoint
    ));
}

// ---------------------------------------------------------------------------
// T-10: one-way conversion; Kernel has no reverse dependency.
// ---------------------------------------------------------------------------

#[test]
fn system_error_to_report_is_a_one_way_read() {
    let error = run_fixture("fuel-loop");
    let before_code = error.code.clone();
    let before_message = error.message.clone();
    let report = ModFaultReport::from_system_error(&error);
    // The conversion reads but does not mutate the Kernel error.
    assert_eq!(error.code, before_code);
    assert_eq!(error.message, before_message);
    // Fields are carried across verbatim.
    assert_eq!(report.code, error.code);
    assert_eq!(report.system_id.as_deref(), Some(error.system.as_str()));
    assert_eq!(report.tick, Some(error.tick.0));
    assert_eq!(
        report.content_digest.as_deref(),
        error.source_digest.as_deref()
    );
    assert_eq!(report.message, error.message);
}

#[test]
fn platform_error_conversion_maps_categories() {
    let manifest = PlatformError::InvalidManifest("bad".to_owned());
    let report = ModFaultReport::from_platform_error(&manifest);
    assert_eq!(report.category, FaultCategory::Manifest);
    assert_eq!(report.phase, FaultPhase::Manifest);
    assert_eq!(report.disposition, Disposition::Stop);
}

// ---------------------------------------------------------------------------
// T-11: budget model is configurable with a config_version.
// ---------------------------------------------------------------------------

#[test]
fn resource_use_carries_config_version_and_is_not_a_frozen_number() {
    let report = ModFaultReport::new("platform.mod.resource.fuel_exceeded", "x").with_resource(
        ResourceUse {
            limit_kind: LimitKind::PerTickFuel,
            limit_value: Some(10_000),
            observed_value: Some(10_001),
            config_version: Some(1),
            exceeded: true,
        },
    );
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["resource"]["config_version"], 1);
    assert_eq!(value["resource"]["limit_kind"], "per_tick_fuel");
    validate_fault_report_value(&value).unwrap();
}

// ---------------------------------------------------------------------------
// Cross-field Schema rules.
// ---------------------------------------------------------------------------

#[test]
fn schema_rejects_rebuild_with_non_rebuild_advice() {
    // cross-field rule 1
    let report = ModFaultReport::new("platform.mod.determinism_violation", "x");
    let mut value = serde_json::to_value(&report).unwrap();
    value["recovery_advice"] = serde_json::json!("retry_call");
    assert!(validate_fault_report_value(&value).is_err());
}

#[test]
fn schema_rejects_wasm_trap_outside_execution_phase() {
    // cross-field rule 3
    let report = ModFaultReport::new("platform.mod.guest_trap.unreachable", "x");
    let mut value = serde_json::to_value(&report).unwrap();
    value["phase"] = serde_json::json!("instantiation");
    assert!(validate_fault_report_value(&value).is_err());
}

#[test]
fn schema_rejects_fix_manifest_without_mod_id() {
    // cross-field rule 2
    let report = ModFaultReport::new("platform.invalid_manifest", "x");
    let value = serde_json::to_value(&report).unwrap();
    // A manifest report has no mod_id by default.
    assert!(value.get("mod_id").is_none());
    assert!(validate_fault_report_value(&value).is_err());
    // Adding a mod_id satisfies the rule.
    let with_id = report.with_mod_id("org.game.sample");
    validate_fault_report_value(&serde_json::to_value(&with_id).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// O-6-1 / O-6-2: placeholders must not fabricate a detection result.
// ---------------------------------------------------------------------------

#[test]
fn determinism_and_migration_placeholders_carry_no_detection_claim() {
    // Only the category + code + decision exist. There is no API that "detects"
    // a determinism violation or migration failure, so these tests assert only
    // the placeholder decision table entries.
    let determinism = ModFaultReport::new("platform.mod.determinism_violation", "placeholder");
    assert_eq!(determinism.category, FaultCategory::Determinism);
    let migration = ModFaultReport::new("platform.mod.migration_failed", "placeholder");
    assert_eq!(migration.category, FaultCategory::Migration);
    // The Schema itself flags them as placeholders in its description.
    assert!(game_platform::MOD_FAULT_REPORT_SCHEMA.contains("O-6-1"));
    assert!(game_platform::MOD_FAULT_REPORT_SCHEMA.contains("O-6-2"));
}

// ---------------------------------------------------------------------------
// Contract anchor: Tick/format_version of the Kernel error are untouched.
// ---------------------------------------------------------------------------

#[test]
fn kernel_system_error_shape_is_unchanged() {
    let error = run_fixture("guest-unreachable");
    // C-7: SystemError keeps its existing Kernel contract shape.
    assert_eq!(error.format_version, 1);
    assert_eq!(error.system, "org.game.guest-unreachable");
    assert_eq!(error.tick, Tick(0));
    assert!(error.source_digest.is_some());
    // The report's version is independent of the Kernel's format_version.
    assert_eq!(FAULT_REPORT_VERSION, 1);
}
