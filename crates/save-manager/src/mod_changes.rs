//! `SAVE-009`: explicit policy decisions for Mod set changes.
//!
//! This module plans changes only. Data mutation still requires SAVE-007,
//! backup protection, and SAVE-010 candidate validation.

use crate::SaveFormatVersion;
use crate::compat::{CompatibilityReport, CompatibilityVerdict};
use crate::partition::ModNamespace;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModChangeRequest {
    Add {
        mod_id: String,
        effect: NewModEffect,
        compatibility: CompatibilityReport,
        initializer_changes_existing_data: bool,
        user_confirmed_semantic_change: bool,
    },
    Update {
        mod_id: String,
        old_schema: String,
        new_schema: String,
        compatibility: CompatibilityReport,
    },
    Remove {
        mod_id: String,
        has_persistent_data: bool,
        required_by_active_mods: bool,
        referenced_by_active_mods: bool,
        request_permanent_data_deletion: bool,
    },
    Replace {
        old_mod_id: String,
        new_mod_id: String,
        explicit_replacement_declaration: bool,
        migration_available: bool,
        reference_mapping_available: bool,
        user_confirmed_data_loss: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NewModEffect {
    PureExtension,
    InitializesExistingWorld,
    ChangesExistingSemantics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModChangeDisposition {
    AllowedDirectly,
    RequiresMigration,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModChangeDecision {
    pub disposition: ModChangeDisposition,
    pub code: &'static str,
    pub affected_mods: Vec<String>,
    pub failed_condition: String,
    pub available_actions: Vec<String>,
    pub needs_backup: bool,
    pub may_lose_data: bool,
    pub may_auto_continue: bool,
}

pub fn plan_mod_change(request: &ModChangeRequest) -> ModChangeDecision {
    let ids: Vec<&str> = match request {
        ModChangeRequest::Add { mod_id, .. }
        | ModChangeRequest::Update { mod_id, .. }
        | ModChangeRequest::Remove { mod_id, .. } => vec![mod_id],
        ModChangeRequest::Replace {
            old_mod_id,
            new_mod_id,
            ..
        } => {
            vec![old_mod_id, new_mod_id]
        }
    };
    if ids.iter().any(|id| ModNamespace::parse(id).is_err()) {
        return blocked(
            "save.mod.invalid_id",
            ids.iter().map(|id| (*id).to_owned()).collect(),
            "Mod IDs must use valid stable namespaces",
            vec!["Correct the Mod identity".into(), "Cancel".into()],
            false,
        );
    }
    if ids.len() == 2 && ids[0] == ids[1] {
        return blocked(
            "save.mod.replacement_identity_conflict",
            ids.iter().map(|id| (*id).to_owned()).collect(),
            "replacement source and destination IDs must differ",
            vec![
                "Correct the replacement declaration".into(),
                "Cancel".into(),
            ],
            false,
        );
    }
    match request {
        ModChangeRequest::Add {
            mod_id,
            effect,
            compatibility,
            initializer_changes_existing_data,
            user_confirmed_semantic_change,
        } => {
            let decision = from_compatibility(compatibility, vec![mod_id.clone()]);
            if decision.disposition == ModChangeDisposition::Blocked {
                return decision;
            }
            match effect {
                NewModEffect::PureExtension if !initializer_changes_existing_data => decision,
                NewModEffect::InitializesExistingWorld | NewModEffect::PureExtension => {
                    require_migration_or_block(
                        decision,
                        compatibility,
                        "new Mod initialization changes existing save data",
                    )
                }
                NewModEffect::ChangesExistingSemantics if *user_confirmed_semantic_change => {
                    require_migration_or_block(
                        decision,
                        compatibility,
                        "semantic change needs an explicit migration",
                    )
                }
                NewModEffect::ChangesExistingSemantics => blocked(
                    "save.mod.semantic_change_confirmation_required",
                    vec![mod_id.clone()],
                    "new Mod changes existing world semantics and was not confirmed",
                    vec!["Review the change".into(), "Cancel".into()],
                    true,
                ),
            }
        }
        ModChangeRequest::Update {
            mod_id,
            old_schema,
            new_schema,
            compatibility,
        } => {
            if SaveFormatVersion::parse(old_schema).is_err()
                || SaveFormatVersion::parse(new_schema).is_err()
            {
                return blocked(
                    "save.mod.schema_version_invalid",
                    vec![mod_id.clone()],
                    "Mod Schema versions must use MAJOR.MINOR syntax",
                    vec!["Correct the Schema declaration".into(), "Cancel".into()],
                    false,
                );
            }
            if old_schema == new_schema {
                let mut decision = from_compatibility(compatibility, vec![mod_id.clone()]);
                if decision.disposition == ModChangeDisposition::AllowedDirectly {
                    decision.code = "save.mod.update_compatible";
                }
                decision
            } else {
                require_migration_or_block(
                    from_compatibility(compatibility, vec![mod_id.clone()]),
                    compatibility,
                    "Mod data Schema changed",
                )
            }
        }
        ModChangeRequest::Remove {
            mod_id,
            has_persistent_data,
            required_by_active_mods,
            referenced_by_active_mods,
            request_permanent_data_deletion,
        } => {
            if *required_by_active_mods {
                return blocked(
                    "save.mod.remove_required_provider",
                    vec![mod_id.clone()],
                    "active Mods still require this Provider",
                    vec![
                        "Keep the Mod installed".into(),
                        "Resolve dependencies".into(),
                        "Cancel".into(),
                    ],
                    true,
                );
            }
            if *referenced_by_active_mods {
                return blocked(
                    "save.mod.remove_referenced_provider",
                    vec![mod_id.clone()],
                    "active Mod data still references this Provider",
                    vec![
                        "Keep the Mod installed".into(),
                        "Migrate references explicitly".into(),
                        "Cancel".into(),
                    ],
                    true,
                );
            }
            if *request_permanent_data_deletion {
                return blocked(
                    "save.mod.data_deletion_requires_separate_recovery",
                    vec![mod_id.clone()],
                    "SAVE-009 never deletes persistent Mod data; explicit data deletion needs a separate backed-up operation",
                    vec!["Remove the Mod and retain its data".into(), "Cancel".into()],
                    true,
                );
            }
            allowed(
                "save.mod.removed_data_retained",
                vec![mod_id.clone()],
                if *has_persistent_data {
                    "Mod removed; its opaque data remains an orphaned partition"
                } else {
                    "Mod removed; no persistent payload exists"
                },
                false,
            )
        }
        ModChangeRequest::Replace {
            old_mod_id,
            new_mod_id,
            explicit_replacement_declaration,
            migration_available,
            reference_mapping_available,
            user_confirmed_data_loss,
        } => {
            let affected = vec![old_mod_id.clone(), new_mod_id.clone()];
            if !explicit_replacement_declaration {
                return blocked(
                    "save.mod.replacement_not_declared",
                    affected,
                    "replacement requires an explicit Provider declaration",
                    vec!["Declare the replacement".into(), "Cancel".into()],
                    true,
                );
            }
            if !migration_available || !reference_mapping_available {
                return blocked(
                    "save.mod.replacement_migration_incomplete",
                    affected,
                    "replacement requires a migration and explicit reference mapping",
                    vec![
                        "Provide a validated migration and reference mapping".into(),
                        "Cancel".into(),
                    ],
                    true,
                );
            }
            if *user_confirmed_data_loss {
                return blocked(
                    "save.mod.replacement_data_loss_unresolved",
                    affected,
                    "confirmation alone cannot authorize unverified data loss",
                    vec!["Provide a validated conversion".into(), "Cancel".into()],
                    true,
                );
            }
            requires_migration(
                "save.mod.replacement_requires_migration",
                affected,
                "replacement must run through a backed-up migration transaction",
            )
        }
    }
}

fn from_compatibility(report: &CompatibilityReport, mods: Vec<String>) -> ModChangeDecision {
    match report.verdict {
        CompatibilityVerdict::Compatible => allowed(
            "save.mod.compatibility_passed",
            mods,
            "compatibility checks passed",
            report.may_auto_continue,
        ),
        CompatibilityVerdict::CompatibleWithMigration => {
            requires_migration(report.reason_code.as_str(), mods, &report.failed_condition)
        }
        CompatibilityVerdict::Blocked | CompatibilityVerdict::Corrupted => blocked(
            report.reason_code.as_str(),
            mods,
            &report.failed_condition,
            report.available_actions.clone(),
            report.needs_backup,
        ),
    }
}

fn require_migration_or_block(
    mut decision: ModChangeDecision,
    compatibility: &CompatibilityReport,
    reason: &str,
) -> ModChangeDecision {
    if compatibility.verdict == CompatibilityVerdict::CompatibleWithMigration {
        decision.disposition = ModChangeDisposition::RequiresMigration;
        decision.code = "save.mod.migration_available";
        decision.failed_condition = reason.to_owned();
        decision.needs_backup = true;
        decision.may_auto_continue = false;
        decision.available_actions = vec![
            "Create a verified backup".into(),
            "Review and run migration".into(),
            "Cancel".into(),
        ];
        decision
    } else {
        blocked(
            "save.mod.migration_required",
            decision.affected_mods,
            reason,
            vec!["Provide a validated migration".into(), "Cancel".into()],
            true,
        )
    }
}

fn allowed(
    code: &'static str,
    affected_mods: Vec<String>,
    reason: &str,
    may_auto_continue: bool,
) -> ModChangeDecision {
    ModChangeDecision {
        disposition: ModChangeDisposition::AllowedDirectly,
        code,
        affected_mods,
        failed_condition: reason.to_owned(),
        available_actions: vec!["Continue".into(), "Cancel".into()],
        needs_backup: false,
        may_lose_data: false,
        may_auto_continue,
    }
}

fn requires_migration(
    code: &'static str,
    affected_mods: Vec<String>,
    reason: &str,
) -> ModChangeDecision {
    ModChangeDecision {
        disposition: ModChangeDisposition::RequiresMigration,
        code,
        affected_mods,
        failed_condition: reason.to_owned(),
        available_actions: vec![
            "Create a verified backup".into(),
            "Review and run migration".into(),
            "Cancel".into(),
        ],
        needs_backup: true,
        may_lose_data: false,
        may_auto_continue: false,
    }
}

fn blocked(
    code: &'static str,
    affected_mods: Vec<String>,
    reason: &str,
    actions: Vec<String>,
    needs_backup: bool,
) -> ModChangeDecision {
    ModChangeDecision {
        disposition: ModChangeDisposition::Blocked,
        code,
        affected_mods,
        failed_condition: reason.to_owned(),
        available_actions: actions,
        needs_backup,
        may_lose_data: false,
        may_auto_continue: false,
    }
}
