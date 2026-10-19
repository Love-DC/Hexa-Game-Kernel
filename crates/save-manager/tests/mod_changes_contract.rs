use std::collections::BTreeSet;

use save_manager::compat::{CompatReasonCode, CompatibilityReport, CompatibilityVerdict};
use save_manager::mod_changes::{
    ModChangeDisposition, ModChangeRequest, NewModEffect, plan_mod_change,
};

fn compatibility(verdict: CompatibilityVerdict) -> CompatibilityReport {
    CompatibilityReport {
        verdict,
        reason_code: match verdict {
            CompatibilityVerdict::Compatible => CompatReasonCode::Compatible,
            CompatibilityVerdict::CompatibleWithMigration => CompatReasonCode::MigrationAvailable,
            CompatibilityVerdict::Blocked => CompatReasonCode::ModMissing,
            CompatibilityVerdict::Corrupted => CompatReasonCode::UnsupportedFormatVersion,
        },
        flags: BTreeSet::new(),
        affected_mods: vec![],
        save_version: None,
        current_version: None,
        failed_condition: "compatibility gate".into(),
        available_actions: vec!["Cancel".into()],
        needs_backup: verdict == CompatibilityVerdict::CompatibleWithMigration,
        may_lose_data: false,
        may_auto_continue: verdict == CompatibilityVerdict::Compatible,
    }
}

#[test]
fn pure_extension_requires_compatibility_and_cannot_initialize_existing_data() {
    let direct = plan_mod_change(&ModChangeRequest::Add {
        mod_id: "org.example.buildings".into(),
        effect: NewModEffect::PureExtension,
        compatibility: compatibility(CompatibilityVerdict::Compatible),
        initializer_changes_existing_data: false,
        user_confirmed_semantic_change: false,
    });
    assert_eq!(direct.disposition, ModChangeDisposition::AllowedDirectly);

    let initialization = plan_mod_change(&ModChangeRequest::Add {
        mod_id: "org.example.buildings".into(),
        effect: NewModEffect::InitializesExistingWorld,
        compatibility: compatibility(CompatibilityVerdict::Compatible),
        initializer_changes_existing_data: true,
        user_confirmed_semantic_change: false,
    });
    assert_eq!(initialization.disposition, ModChangeDisposition::Blocked);
    assert!(initialization.needs_backup);
}

#[test]
fn migration_and_semantic_confirmation_are_explicit_gates() {
    let migration = plan_mod_change(&ModChangeRequest::Update {
        mod_id: "org.example.economy".into(),
        old_schema: "1.0".into(),
        new_schema: "2.0".into(),
        compatibility: compatibility(CompatibilityVerdict::CompatibleWithMigration),
    });
    assert_eq!(
        migration.disposition,
        ModChangeDisposition::RequiresMigration
    );
    assert!(migration.needs_backup);
    assert!(!migration.may_auto_continue);

    let semantic = plan_mod_change(&ModChangeRequest::Add {
        mod_id: "org.example.rules".into(),
        effect: NewModEffect::ChangesExistingSemantics,
        compatibility: compatibility(CompatibilityVerdict::CompatibleWithMigration),
        initializer_changes_existing_data: true,
        user_confirmed_semantic_change: false,
    });
    assert_eq!(
        semantic.code,
        "save.mod.semantic_change_confirmation_required"
    );
}

#[test]
fn removal_retains_payload_and_blocks_dependents_references_or_delete() {
    let retained = plan_mod_change(&ModChangeRequest::Remove {
        mod_id: "org.example.economy".into(),
        has_persistent_data: true,
        required_by_active_mods: false,
        referenced_by_active_mods: false,
        request_permanent_data_deletion: false,
    });
    assert_eq!(retained.disposition, ModChangeDisposition::AllowedDirectly);
    assert_eq!(retained.code, "save.mod.removed_data_retained");

    for request in [
        ModChangeRequest::Remove {
            mod_id: "org.example.economy".into(),
            has_persistent_data: true,
            required_by_active_mods: true,
            referenced_by_active_mods: false,
            request_permanent_data_deletion: false,
        },
        ModChangeRequest::Remove {
            mod_id: "org.example.economy".into(),
            has_persistent_data: true,
            required_by_active_mods: false,
            referenced_by_active_mods: true,
            request_permanent_data_deletion: false,
        },
        ModChangeRequest::Remove {
            mod_id: "org.example.economy".into(),
            has_persistent_data: true,
            required_by_active_mods: false,
            referenced_by_active_mods: false,
            request_permanent_data_deletion: true,
        },
    ] {
        assert_eq!(
            plan_mod_change(&request).disposition,
            ModChangeDisposition::Blocked
        );
    }
}

#[test]
fn replacement_requires_declaration_migration_and_reference_mapping() {
    let incomplete = plan_mod_change(&ModChangeRequest::Replace {
        old_mod_id: "org.example.old".into(),
        new_mod_id: "org.example.new".into(),
        explicit_replacement_declaration: true,
        migration_available: true,
        reference_mapping_available: false,
        user_confirmed_data_loss: false,
    });
    assert_eq!(incomplete.disposition, ModChangeDisposition::Blocked);

    let ready = plan_mod_change(&ModChangeRequest::Replace {
        old_mod_id: "org.example.old".into(),
        new_mod_id: "org.example.new".into(),
        explicit_replacement_declaration: true,
        migration_available: true,
        reference_mapping_available: true,
        user_confirmed_data_loss: false,
    });
    assert_eq!(ready.disposition, ModChangeDisposition::RequiresMigration);
}
