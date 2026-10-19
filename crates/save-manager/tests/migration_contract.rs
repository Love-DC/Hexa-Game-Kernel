use std::collections::{BTreeMap, BTreeSet};

use kernel_core::Tick;
use save_manager::compat::{MigrationHop, MigrationPath};
use save_manager::integrity;
use save_manager::loader::inspect_save;
use save_manager::migration::{
    DescriptorField, MigrationDataScope, MigrationDomain, MigrationLookupError, MigrationRegistry,
    MigrationRegistryError, MigrationTargets, MigrationVerification, MigratorDescriptor,
    build_migration_plan, to_compatibility_path,
};
use save_manager::partition::{PartitionPayload, SaveLayout, SavePartition, SavePartitionId};
use save_manager::writer;
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion as V, SaveManifest, SaveModEntry, SimulationConfig,
};

fn domain() -> MigrationDomain {
    MigrationDomain::Mod {
        mod_id: "org.example.mod".into(),
    }
}

fn descriptor(id: &str, from: (u32, u32), to: (u32, u32)) -> MigratorDescriptor {
    MigratorDescriptor {
        id: id.into(),
        domain: domain(),
        from_schema: V::new(from.0, from.1),
        to_schema: V::new(to.0, to.1),
        migrator_version: "1.0.0".into(),
        requires: BTreeSet::new(),
        input_scope: MigrationDataScope::default(),
        output_scope: MigrationDataScope::default(),
        changes_world_data: false,
        has_irreversible_ops: false,
        resource_budget: BTreeMap::from([("memory.bytes".into(), 1024)]),
        verification: MigrationVerification::GoldenDigest,
    }
}

fn registry(descriptors: Vec<MigratorDescriptor>) -> MigrationRegistry {
    MigrationRegistry::build(descriptors).expect("valid registry")
}

fn manifest() -> SaveManifest {
    SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "migration.contract".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.example.game".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "1.0".into(),
        world_schema_version: "1.0".into(),
        logical_tick: Tick(0),
        mods: vec![SaveModEntry {
            id: "org.example.mod".into(),
            version: "1.0.0".into(),
            schema_version: "1.0".into(),
        }],
        simulation_config: SimulationConfig::new(),
        integrity: Integrity {
            algorithm: "sha256".into(),
            root_hash: "0".repeat(64),
        },
    }
}

fn inspected_save() -> save_manager::loader::InspectedSave {
    let system = writer::encode_system_manifest(&[]).expect("system manifest");
    let first = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, manifest().to_bytes()),
        SavePartition::new(SavePartitionId::World, b"world".to_vec()),
        SavePartition::new(SavePartitionId::Metadata, system),
        SavePartition::new(SavePartitionId::Integrity, Vec::new()),
    ]);
    let generated = integrity::generate(&first).expect("digest");
    let mut final_manifest = manifest();
    final_manifest.integrity.root_hash = generated.root;
    let final_layout = SaveLayout::new(
        first
            .partitions
            .into_iter()
            .map(|mut partition| {
                match partition.id {
                    SavePartitionId::Manifest => {
                        partition.payload = PartitionPayload::new(final_manifest.to_bytes())
                    }
                    SavePartitionId::Integrity => {
                        partition.payload = PartitionPayload::new(generated.checksums.clone())
                    }
                    _ => {}
                }
                partition
            })
            .collect(),
    );
    inspect_save(&final_layout.to_bytes().expect("container")).expect("save inspection")
}

#[test]
fn registration_rejects_duplicate_ids_conflicting_edges_and_invalid_descriptors() {
    let duplicate = MigrationRegistry::build(vec![
        descriptor("same", (1, 0), (2, 0)),
        descriptor("same", (2, 0), (3, 0)),
    ])
    .unwrap_err();
    assert!(matches!(
        duplicate,
        MigrationRegistryError::DuplicateMigratorId { .. }
    ));
    assert_eq!(duplicate.code(), "save.migration.duplicate_migrator_id");
    let conflict = MigrationRegistry::build(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (1, 0), (2, 0)),
    ])
    .unwrap_err();
    assert!(matches!(
        conflict,
        MigrationRegistryError::ConflictingMigrationEdge { .. }
    ));
    assert_eq!(conflict.code(), "save.migration.conflicting_edge");
    let mut bad = descriptor("bad", (1, 0), (2, 0));
    bad.migrator_version = "not-semver".into();
    let error = MigrationRegistry::build(vec![bad]).unwrap_err();
    assert!(matches!(
        error,
        MigrationRegistryError::InvalidDescriptor {
            field: DescriptorField::MigratorVersion,
            ..
        }
    ));
    assert_eq!(error.code(), "save.migration.invalid_descriptor");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&error).unwrap()).unwrap()
            ["code"],
        "save.migration.invalid_descriptor"
    );
    let mut empty_id = descriptor("blank", (1, 0), (2, 0));
    empty_id.id = "  ".into();
    assert!(matches!(
        MigrationRegistry::build(vec![empty_id]),
        Err(MigrationRegistryError::InvalidDescriptor {
            field: DescriptorField::Id,
            ..
        })
    ));
    let mut empty_owner = descriptor("owner", (1, 0), (2, 0));
    empty_owner.domain = MigrationDomain::Mod { mod_id: " ".into() };
    assert!(matches!(
        MigrationRegistry::build(vec![empty_owner]),
        Err(MigrationRegistryError::InvalidDescriptor {
            field: DescriptorField::Owner,
            ..
        })
    ));
    assert!(matches!(
        MigrationRegistry::build(vec![descriptor("identity", (1, 0), (1, 0))]),
        Err(MigrationRegistryError::InvalidDescriptor {
            field: DescriptorField::ToSchema,
            ..
        })
    ));
}

#[test]
fn explicit_reverse_edges_are_allowed_and_unique_chain_is_ordered() {
    let reg = registry(vec![
        descriptor("forward", (1, 0), (2, 0)),
        descriptor("reverse", (2, 0), (1, 0)),
    ]);
    let chain = reg
        .find_path(&domain(), V::new(1, 0), V::new(2, 0), &BTreeSet::new())
        .unwrap();
    assert_eq!(chain.hops[0].migrator_id, "forward");
    let empty = reg
        .find_path(&domain(), V::new(2, 0), V::new(2, 0), &BTreeSet::new())
        .unwrap();
    assert!(empty.hops.is_empty());
    let multi = registry(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (3, 0)),
        descriptor("c", (3, 0), (4, 0)),
    ]);
    let chain = multi
        .find_path(&domain(), V::new(1, 0), V::new(4, 0), &BTreeSet::new())
        .unwrap();
    assert_eq!(
        chain
            .hops
            .iter()
            .map(|hop| hop.migrator_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
}

#[test]
fn ambiguity_has_bounded_sorted_diagnostics_and_stable_error_codes() {
    let reg = registry(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (4, 0)),
        descriptor("c", (1, 0), (3, 0)),
        descriptor("d", (3, 0), (4, 0)),
        descriptor("e", (1, 0), (5, 0)),
        descriptor("f", (5, 0), (4, 0)),
        descriptor("g", (1, 0), (6, 0)),
        descriptor("h", (6, 0), (4, 0)),
    ]);
    let error = reg
        .find_path(&domain(), V::new(1, 0), V::new(4, 0), &BTreeSet::new())
        .unwrap_err();
    match &error {
        MigrationLookupError::AmbiguousMigrationPath {
            discovered_chains,
            truncated,
            ..
        } => {
            assert_eq!(discovered_chains.len(), 3);
            assert!(*truncated);
            assert_eq!(discovered_chains[0].hops[0].migrator_id, "a");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(error.code(), "save.migration.ambiguous_path");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&error).unwrap()).unwrap()
            ["code"],
        error.code()
    );
    let exactly_two = registry(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (4, 0)),
        descriptor("c", (1, 0), (3, 0)),
        descriptor("d", (3, 0), (4, 0)),
    ]);
    assert!(matches!(
        exactly_two.find_path(&domain(), V::new(1, 0), V::new(4, 0), &BTreeSet::new()),
        Err(MigrationLookupError::AmbiguousMigrationPath {
            truncated: false,
            ..
        })
    ));
    let exactly_three = registry(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (4, 0)),
        descriptor("c", (1, 0), (3, 0)),
        descriptor("d", (3, 0), (4, 0)),
        descriptor("e", (1, 0), (5, 0)),
        descriptor("f", (5, 0), (4, 0)),
    ]);
    assert!(
        matches!(exactly_three.find_path(&domain(), V::new(1, 0), V::new(4, 0), &BTreeSet::new()), Err(MigrationLookupError::AmbiguousMigrationPath { discovered_chains, truncated: true, .. }) if discovered_chains.len() == 3)
    );
    let cycle = registry(vec![
        descriptor("ab", (1, 0), (2, 0)),
        descriptor("bc", (2, 0), (3, 0)),
        descriptor("cb", (3, 0), (2, 0)),
        descriptor("bd", (2, 0), (4, 0)),
        descriptor("ac", (1, 0), (3, 0)),
        descriptor("cd", (3, 0), (4, 0)),
    ]);
    assert!(matches!(
        cycle.find_path(&domain(), V::new(1, 0), V::new(4, 0), &BTreeSet::new()),
        Err(MigrationLookupError::AmbiguousMigrationPath { .. })
    ));
}

#[test]
fn missing_and_incompatible_paths_are_distinguished_and_pruned() {
    let mut gated = descriptor("gated", (1, 0), (2, 0));
    gated.requires.insert("org.example.dep".into());
    let reg = registry(vec![gated]);
    let err = reg
        .find_path(&domain(), V::new(1, 0), V::new(2, 0), &BTreeSet::new())
        .unwrap_err();
    assert_eq!(err.code(), "save.migration.incompatible_step");
    assert!(matches!(err, MigrationLookupError::IncompatibleStep { .. }));
    let missing = reg
        .find_path(&domain(), V::new(1, 0), V::new(3, 0), &BTreeSet::new())
        .unwrap_err();
    assert_eq!(missing.code(), "save.migration.no_path");
    assert!(matches!(
        missing,
        MigrationLookupError::NoMigrationPath { .. }
    ));
    let mut gated = descriptor("gated", (1, 0), (2, 0));
    gated.requires.insert("org.example.dep".into());
    let alternative = registry(vec![
        gated,
        descriptor("usable", (1, 0), (3, 0)),
        descriptor("finish", (3, 0), (2, 0)),
    ]);
    assert_eq!(
        alternative
            .find_path(&domain(), V::new(1, 0), V::new(2, 0), &BTreeSet::new())
            .unwrap()
            .hops[0]
            .migrator_id,
        "usable"
    );
}

#[test]
fn unique_path_search_exhausts_irrelevant_branches() {
    let mut graph = vec![descriptor("only", (1, 0), (9, 0))];
    for index in 0..24 {
        let branch = 100 + index;
        graph.push(descriptor(&format!("dead-{index}-a"), (1, 0), (branch, 0)));
        graph.push(descriptor(
            &format!("dead-{index}-b"),
            (branch, 0),
            (branch + 100, 0),
        ));
    }
    let reg = registry(graph);
    let chain = reg
        .find_path(&domain(), V::new(1, 0), V::new(9, 0), &BTreeSet::new())
        .unwrap();
    assert_eq!(
        chain
            .hops
            .iter()
            .map(|hop| hop.migrator_id.as_str())
            .collect::<Vec<_>>(),
        ["only"]
    );
}

#[test]
fn registration_order_domains_and_compatibility_adapter_are_deterministic() {
    let first = vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (3, 0)),
    ];
    let second = first.iter().cloned().rev().collect();
    let left = registry(first);
    let right = registry(second);
    assert_eq!(
        serde_json::to_vec(&left).unwrap(),
        serde_json::to_vec(&right).unwrap()
    );
    let installed = BTreeSet::new();
    let l = left
        .find_path(&domain(), V::new(1, 0), V::new(3, 0), &installed)
        .unwrap();
    let r = right
        .find_path(&domain(), V::new(1, 0), V::new(3, 0), &installed)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&l).unwrap(),
        serde_json::to_vec(&r).unwrap()
    );
    assert!(
        matches!(to_compatibility_path(Ok(&l)), MigrationPath::Available(hops) if hops == vec![MigrationHop { from_version: V::new(1, 0), to_version: V::new(2, 0), migrator_id: "a".into() }, MigrationHop { from_version: V::new(2, 0), to_version: V::new(3, 0), migrator_id: "b".into() }])
    );
    let unrelated = MigrationDomain::World {
        game_definition_id: "org.example.game".into(),
    };
    assert!(matches!(
        left.find_path(&unrelated, V::new(1, 0), V::new(3, 0), &installed),
        Err(MigrationLookupError::NoMigrationPath { .. })
    ));
    assert_eq!(
        to_compatibility_path(Err(&MigrationLookupError::NoMigrationPath {
            domain: domain(),
            from: V::new(1, 0),
            to: V::new(3, 0)
        })),
        MigrationPath::Unavailable
    );
}

#[test]
fn plan_is_repeatable_carries_descriptor_snapshot_and_keeps_chain_order() {
    let save = inspected_save();
    let reg = registry(vec![
        descriptor("a", (1, 0), (2, 0)),
        descriptor("b", (2, 0), (3, 0)),
    ]);
    let targets = MigrationTargets {
        world_schema_version: V::new(1, 0),
        mod_schema_versions: BTreeMap::from([("org.example.mod".into(), V::new(3, 0))]),
    };
    let installed = BTreeSet::from(["org.example.mod".into()]);
    let one = build_migration_plan(&save, &reg, &targets, &installed).unwrap();
    let two = build_migration_plan(&save, &reg, &targets, &installed).unwrap();
    assert_eq!(
        serde_json::to_vec(&one).unwrap(),
        serde_json::to_vec(&two).unwrap()
    );
    assert_eq!(one.save_id, "migration.contract");
    assert_eq!(one.source_root_hash, save.manifest().integrity.root_hash);
    assert_eq!(
        one.steps
            .iter()
            .map(|step| step.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        one.steps[0].descriptor.resource_budget["memory.bytes"],
        1024
    );
    assert_eq!(
        one.steps[0].descriptor.verification,
        MigrationVerification::GoldenDigest
    );
    let missing_target = MigrationTargets {
        world_schema_version: V::new(1, 0),
        mod_schema_versions: BTreeMap::new(),
    };
    assert!(matches!(
        build_migration_plan(&save, &reg, &missing_target, &installed),
        Err(save_manager::migration::MigrationPlanError::MissingInstalledModTarget { mod_id }) if mod_id == "org.example.mod"
    ));
}

#[test]
fn migration_module_contains_no_io_or_environment_access() {
    let source = include_str!("../src/migration.rs");
    for forbidden in [
        "std::fs",
        "std::net",
        "std::process",
        "std::time",
        "SystemTime",
        "std::env",
    ] {
        assert!(
            !source.contains(forbidden),
            "migration.rs contains forbidden token {forbidden}"
        );
    }
}
