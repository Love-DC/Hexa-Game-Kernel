//! Restricted Wasm execution for SAVE-007 migrators.
//!
//! The guest ABI is intentionally small: one exported `memory` of exactly three
//! 64 KiB pages and one exported function
//! `migrate(i32 input_ptr, i32 input_len, i32 output_ptr, i32 output_capacity)
//! -> i32 output_len`. Input and output are canonical serde JSON encodings of
//! SAVE-007's scoped `MigrationInput`/`MigrationOutput`. Migrators have no
//! imports, WASI, host APIs, filesystem, network, process, clock, or randomness.
//! Each call starts from a new Store and receives a fixed fuel budget.

use std::collections::BTreeMap;

use save_manager::migration::PlannedMigration;
use save_manager::transaction::{MigrationInput, MigrationOutput, MigrationRunner, RunnerFailure};
use sha2::{Digest, Sha256};
use wasmtime::{
    Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, WasmFeatures,
};

use crate::MigrationDeterminismMonitor;

// Page 0 is reserved for guest static data; pages 1 and 2 are ABI scratch
// buffers owned by the host. This prevents the host input copy from corrupting
// a guest's initialized data segment.
const PAGE_BYTES: usize = 64 * 1024;
const MEMORY_BYTES: usize = PAGE_BYTES * 3;
const INPUT_OFFSET: usize = PAGE_BYTES;
const OUTPUT_OFFSET: usize = PAGE_BYTES * 2;
const BUFFER_BYTES: usize = PAGE_BYTES;
const FUEL_PER_MIGRATION: u64 = 1_000_000;
const ABI_VERSION: &[u8] = b"game-kernel-migration-abi-v1\0";

/// Published JSON shape of the bytes exchanged through the migration ABI.
pub const MIGRATION_ABI_V1_SCHEMA_JSON: &str =
    include_str!("../../../schemas/migration-abi-v1.schema.json");

struct MigrationStoreState {
    limits: StoreLimits,
}

/// Creates the same restricted Wasmtime engine profile used by Platform,
/// with fuel metering and the project's pinned stack ceiling.
pub fn migration_engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .max_wasm_stack(256 * 1024)
        .wasm_simd(false)
        .wasm_features(WasmFeatures::RELAXED_SIMD, false);
    Engine::new(&config).map_err(|error| error.to_string())
}

/// Compiled Wasm modules indexed by the exact frozen migrator descriptor ID.
/// No guest is instantiated until its SAVE-007 step is invoked.
pub struct WasmMigrationRunner {
    engine: Engine,
    modules: BTreeMap<String, (Module, String)>,
    determinism: MigrationDeterminismMonitor,
}

impl WasmMigrationRunner {
    /// Validates and compiles each module. Any import is rejected up front;
    /// module identity is the SHA-256 of the supplied bytes.
    pub fn new(
        engine: Engine,
        modules: BTreeMap<String, Vec<u8>>,
        observation_capacity: usize,
    ) -> Result<Self, String> {
        let mut compiled = BTreeMap::new();
        for (id, bytes) in modules {
            if id.is_empty() || id.len() > 128 || compiled.contains_key(&id) {
                return Err("invalid or duplicate migration module ID".to_owned());
            }
            let module = Module::new(&engine, &bytes).map_err(|error| error.to_string())?;
            if module.imports().next().is_some() {
                return Err(format!("migrator `{id}` must not import host capabilities"));
            }
            let digest = format!("{:x}", Sha256::digest(&bytes));
            compiled.insert(id, (module, digest));
        }
        Ok(Self {
            engine,
            modules: compiled,
            determinism: MigrationDeterminismMonitor::new(observation_capacity),
        })
    }

    fn invoke(&self, module: &Module, input: &[u8]) -> Result<Vec<u8>, RunnerFailure> {
        if input.len() > BUFFER_BYTES {
            return Err(failure(
                "save.migration.input_too_large",
                "migration input exceeds ABI buffer",
            ));
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEMORY_BYTES)
            .instances(1)
            .memories(1)
            .tables(1)
            .table_elements(0)
            .build();
        let mut store = Store::new(&self.engine, MigrationStoreState { limits });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(FUEL_PER_MIGRATION)
            .map_err(|error| failure("save.migration.runtime_setup", error.to_string()))?;
        let linker = Linker::<MigrationStoreState>::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|error| failure("save.migration.instantiate_failed", error.to_string()))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| failure("save.migration.abi_invalid", "missing exported memory"))?;
        if memory.data_size(&store) != MEMORY_BYTES {
            return Err(failure(
                "save.migration.abi_invalid",
                "exported memory must be exactly three 64 KiB pages",
            ));
        }
        memory
            .write(&mut store, INPUT_OFFSET, input)
            .map_err(|error| failure("save.migration.memory_write_failed", error.to_string()))?;
        let migrate = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "migrate")
            .map_err(|error| failure("save.migration.abi_invalid", error.to_string()))?;
        let output_len = migrate
            .call(
                &mut store,
                (
                    INPUT_OFFSET as i32,
                    i32::try_from(input.len()).unwrap_or(i32::MAX),
                    OUTPUT_OFFSET as i32,
                    BUFFER_BYTES as i32,
                ),
            )
            .map_err(|error| failure("save.migration.guest_failed", error.to_string()))?;
        let output_len = usize::try_from(output_len).map_err(|_| {
            failure(
                "save.migration.output_invalid",
                "guest returned a negative output length",
            )
        })?;
        if output_len > BUFFER_BYTES {
            return Err(failure(
                "save.migration.output_too_large",
                "guest output exceeds ABI buffer",
            ));
        }
        let mut output = vec![0; output_len];
        memory
            .read(&store, OUTPUT_OFFSET, &mut output)
            .map_err(|error| failure("save.migration.memory_read_failed", error.to_string()))?;
        Ok(output)
    }
}

impl MigrationRunner for WasmMigrationRunner {
    fn run(
        &mut self,
        step: &PlannedMigration,
        input: MigrationInput,
    ) -> Result<MigrationOutput, RunnerFailure> {
        let (module, code_digest) = self.modules.get(&step.descriptor.id).ok_or_else(|| {
            failure(
                "save.migration.module_missing",
                format!("no Wasm module registered for `{}`", step.descriptor.id),
            )
        })?;
        let input_bytes = serde_json::to_vec(&input)
            .map_err(|error| failure("save.migration.input_invalid", error.to_string()))?;
        let descriptor_bytes = serde_json::to_vec(&step.descriptor)
            .map_err(|error| failure("save.migration.descriptor_invalid", error.to_string()))?;
        let mut context = Vec::with_capacity(
            ABI_VERSION.len() + code_digest.len() + descriptor_bytes.len() + input_bytes.len(),
        );
        context.extend_from_slice(ABI_VERSION);
        for field in [
            code_digest.as_bytes(),
            descriptor_bytes.as_slice(),
            input_bytes.as_slice(),
        ] {
            context.extend_from_slice(
                &u64::try_from(field.len())
                    .map_err(|_| {
                        failure("save.migration.input_invalid", "context field too large")
                    })?
                    .to_be_bytes(),
            );
            context.extend_from_slice(field);
        }

        // Run from two fresh stores so accidental state, process state, or a
        // hidden nondeterministic source cannot be mistaken for a stable result.
        let first = self.invoke(module, &input_bytes)?;
        self.determinism
            .observe(
                migration_owner_id(&step.descriptor),
                &step.descriptor.id,
                &context,
                &first,
            )
            .map_err(|error| failure("platform.mod.determinism_violation", format!("{error:?}")))?;
        let second = self.invoke(module, &input_bytes)?;
        self.determinism
            .observe(
                migration_owner_id(&step.descriptor),
                &step.descriptor.id,
                &context,
                &second,
            )
            .map_err(|error| failure("platform.mod.determinism_violation", format!("{error:?}")))?;
        if first != second {
            return Err(failure(
                "platform.mod.determinism_violation",
                "same migrator, descriptor, and input produced different output bytes",
            ));
        }
        serde_json::from_slice(&first)
            .map_err(|error| failure("save.migration.output_invalid", error.to_string()))
    }
}

fn failure(code: &'static str, message: impl Into<String>) -> RunnerFailure {
    RunnerFailure {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn migration_owner_id(descriptor: &save_manager::migration::MigratorDescriptor) -> &str {
    match &descriptor.domain {
        save_manager::migration::MigrationDomain::World { game_definition_id } => {
            game_definition_id
        }
        save_manager::migration::MigrationDomain::Mod { mod_id } => mod_id,
    }
}
