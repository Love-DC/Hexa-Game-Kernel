use kernel_core::Tick;
use save_manager::integrity;
use save_manager::loader::inspect_save;
use save_manager::partition::{
    ModNamespace, PartitionPayload, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::references::{
    MigrationValidationPipeline, ModReferenceExtractor, ModReferenceSnapshot,
    ReferenceExtractionFailure, ReferenceExtractorRegistry, ReferenceTarget, StableReference,
    WorldInvariantValidator, validate_cross_mod_references,
};
use save_manager::transaction::{MigrationCandidateValidator, ValidationFailure};
use save_manager::writer;
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion as V, SaveManifest, SaveModEntry, SimulationConfig,
};

fn build_save(mods: &[(&str, &str)], world: &[u8]) -> (SaveManifest, SaveLayout) {
    let mut manifest = SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "save.references.contract".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.example.game".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "1.0".into(),
        world_schema_version: "1.0".into(),
        logical_tick: Tick(4),
        mods: mods
            .iter()
            .map(|(id, _)| SaveModEntry {
                id: (*id).into(),
                version: "1.0.0".into(),
                schema_version: "1.0".into(),
            })
            .collect(),
        simulation_config: SimulationConfig::new(),
        integrity: Integrity {
            algorithm: "sha256".into(),
            root_hash: "0".repeat(64),
        },
    };
    let mod_parts: Vec<_> = mods
        .iter()
        .map(|(id, payload)| {
            SavePartition::new(
                SavePartitionId::Mod(ModNamespace::parse(id).unwrap()),
                payload.as_bytes(),
            )
        })
        .collect();
    let mut partitions = vec![
        SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
        SavePartition::new(SavePartitionId::World, world),
    ];
    partitions.extend(mod_parts);
    partitions.push(SavePartition::new(
        SavePartitionId::Metadata,
        writer::encode_system_manifest(&[]).unwrap(),
    ));
    partitions.push(SavePartition::new(SavePartitionId::Integrity, Vec::new()));
    let mut layout = SaveLayout::new(partitions);
    let generated = integrity::generate(&layout).unwrap();
    manifest.integrity.root_hash = generated.root;
    layout = SaveLayout::new(
        layout
            .partitions()
            .iter()
            .cloned()
            .map(|mut part| {
                match part.id {
                    SavePartitionId::Manifest => {
                        part.payload = PartitionPayload::new(manifest.to_bytes())
                    }
                    SavePartitionId::Integrity => {
                        part.payload = PartitionPayload::new(generated.checksums.clone())
                    }
                    _ => {}
                }
                part
            })
            .collect(),
    );
    (manifest, layout)
}

#[derive(Clone)]
struct FixtureExtractor {
    snapshot: ModReferenceSnapshot,
}
impl ModReferenceExtractor for FixtureExtractor {
    fn extract(
        &self,
        _owner: &str,
        _payload: &[u8],
    ) -> Result<ModReferenceSnapshot, ReferenceExtractionFailure> {
        Ok(self.snapshot.clone())
    }
}

fn valid_registry() -> ReferenceExtractorRegistry {
    let mut registry = ReferenceExtractorRegistry::default();
    registry
        .register(
            "org.example.a",
            Box::new(FixtureExtractor {
                snapshot: ModReferenceSnapshot {
                    exports: vec![],
                    references: vec![StableReference {
                        source_id: "building-1".into(),
                        provider_mod: "org.example.b".into(),
                        target_type: "resource".into(),
                        stable_id: "iron".into(),
                    }],
                },
            }),
        )
        .unwrap();
    registry
        .register(
            "org.example.b",
            Box::new(FixtureExtractor {
                snapshot: ModReferenceSnapshot {
                    exports: vec![ReferenceTarget {
                        provider_mod: "org.example.b".into(),
                        target_type: "resource".into(),
                        stable_id: "iron".into(),
                    }],
                    references: vec![],
                },
            }),
        )
        .unwrap();
    registry
}

#[test]
fn references_resolve_only_with_explicit_provider_type_and_stable_id() {
    let (manifest, layout) =
        build_save(&[("org.example.a", "A"), ("org.example.b", "B")], b"world");
    let report = validate_cross_mod_references(&manifest, &layout, &valid_registry()).unwrap();
    assert_eq!(report.checked_mods, 2);
    assert_eq!(report.checked_references, 1);
    assert_eq!(report.exported_targets, 1);
    let inspected = inspect_save(&layout.to_bytes().unwrap()).unwrap();
    assert_eq!(
        save_manager::references::inspect_references(&inspected, &valid_registry()).unwrap(),
        report
    );
}

#[test]
fn missing_provider_and_target_are_distinct_fail_closed_results() {
    let (manifest, layout) = build_save(&[("org.example.a", "A")], b"world");
    let mut registry = ReferenceExtractorRegistry::default();
    registry
        .register(
            "org.example.a",
            Box::new(FixtureExtractor {
                snapshot: ModReferenceSnapshot {
                    references: vec![StableReference {
                        source_id: "building-1".into(),
                        provider_mod: "org.example.b".into(),
                        target_type: "resource".into(),
                        stable_id: "iron".into(),
                    }],
                    exports: vec![],
                },
            }),
        )
        .unwrap();
    assert_eq!(
        validate_cross_mod_references(&manifest, &layout, &registry)
            .unwrap_err()
            .code,
        "save.reference.provider_missing"
    );

    let (manifest, layout) =
        build_save(&[("org.example.a", "A"), ("org.example.b", "B")], b"world");
    // Replace B's extractor with an empty export set to model a missing object.
    let mut registry = ReferenceExtractorRegistry::default();
    registry
        .register(
            "org.example.a",
            Box::new(FixtureExtractor {
                snapshot: ModReferenceSnapshot {
                    references: vec![StableReference {
                        source_id: "building-1".into(),
                        provider_mod: "org.example.b".into(),
                        target_type: "resource".into(),
                        stable_id: "iron".into(),
                    }],
                    exports: vec![],
                },
            }),
        )
        .unwrap();
    registry
        .register(
            "org.example.b",
            Box::new(FixtureExtractor {
                snapshot: ModReferenceSnapshot::default(),
            }),
        )
        .unwrap();
    assert_eq!(
        validate_cross_mod_references(&manifest, &layout, &registry)
            .unwrap_err()
            .code,
        "save.reference.target_missing"
    );
}

struct AcceptWorld;
impl WorldInvariantValidator for AcceptWorld {
    fn validate_world(
        &mut self,
        _manifest: &SaveManifest,
        world: &[u8],
    ) -> Result<(), ValidationFailure> {
        if world == b"world" {
            Ok(())
        } else {
            Err(ValidationFailure {
                code: "save.world.invalid".into(),
                message: "world rejected".into(),
            })
        }
    }
}

#[test]
fn migration_validation_pipeline_requires_world_and_reference_checks() {
    let (manifest, layout) =
        build_save(&[("org.example.a", "A"), ("org.example.b", "B")], b"world");
    let mut validator = MigrationValidationPipeline::new(Box::new(AcceptWorld), valid_registry());
    validator.validate(&manifest, &layout).unwrap();
    let (manifest, layout) = build_save(
        &[("org.example.a", "A"), ("org.example.b", "B")],
        b"bad-world",
    );
    assert_eq!(
        validator.validate(&manifest, &layout).unwrap_err().code,
        "save.world.invalid"
    );
}

#[test]
fn duplicate_extractor_registration_does_not_replace_first_registration() {
    let mut registry = valid_registry();
    let duplicate = registry.register(
        "org.example.a",
        Box::new(FixtureExtractor {
            snapshot: ModReferenceSnapshot::default(),
        }),
    );
    assert_eq!(
        duplicate.unwrap_err().code,
        "save.reference.duplicate_extractor"
    );
    let (manifest, layout) =
        build_save(&[("org.example.a", "A"), ("org.example.b", "B")], b"world");
    assert_eq!(
        validate_cross_mod_references(&manifest, &layout, &registry)
            .unwrap()
            .checked_references,
        1
    );
}
