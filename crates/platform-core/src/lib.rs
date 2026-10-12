//! Platform layer: package discovery, strict manifest validation, deterministic
//! dependency locking, and a restricted Wasm-to-Kernel adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use kernel_core::{Command, ComponentValue, System, SystemError, Tick, World};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::{
    Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, WasmFeatures,
};

const MANIFEST_NAME: &str = "mod.json";
const MANIFEST_VERSION: u32 = 1;
const KERNEL_API_VERSION: &str = "0.1";
const WASMTIME_RUNTIME_ID: &str = "wasmtime-48.0.5";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
const MAX_LINEAR_MEMORY_BYTES: usize = 64 * 1024;
const FUEL_PER_TICK: u64 = 10_000;
const MAX_WASM_STACK_BYTES: usize = 256 * 1024;
const COUNTER_RESOURCE: &str = "org.game/counter";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModFailureCode {
    RuntimeSetup,
    ExecutionFailed,
    InvalidOutput,
    InvalidWorldState,
}

impl ModFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeSetup => "platform.mod.runtime_setup",
            Self::ExecutionFailed => "platform.mod.execution_failed",
            Self::InvalidOutput => "platform.mod.invalid_output",
            Self::InvalidWorldState => "platform.mod.invalid_world_state",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModDependency {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModManifest {
    pub manifest_version: u32,
    pub id: String,
    pub version: String,
    pub kernel_api: String,
    pub entrypoint: String,
    pub requested_permissions: BTreeSet<String>,
    #[serde(default)]
    pub dependencies: Vec<ModDependency>,
}

#[derive(Clone, Debug)]
pub struct ModPackage {
    root: PathBuf,
    pub manifest: ModManifest,
    module_bytes: Vec<u8>,
    pub content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LockedMod {
    pub id: String,
    pub version: String,
    pub content_digest: String,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LoadPlan {
    pub format_version: u32,
    pub kernel_api: String,
    pub runtime: String,
    pub mods: Vec<LockedMod>,
    pub config_digest: String,
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("I/O error at `{path}`: {message}")]
    Io { path: PathBuf, message: String },
    #[error("invalid Mod manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid Mod package: {0}")]
    InvalidPackage(String),
    #[error("Mod `{mod_id}` requested permission `{permission}` which was not granted")]
    PermissionDenied { mod_id: String, permission: String },
    #[error("Mod `{mod_id}` imports an API requiring unrequested permission `{permission}`")]
    PermissionNotRequested { mod_id: String, permission: String },
    #[error("dependency resolution failed: {0}")]
    DependencyResolution(String),
    #[error("Wasm runtime setup failed: {0}")]
    RuntimeSetup(String),
    #[error("Wasm module for Mod `{mod_id}` is invalid: {message}")]
    WasmModule { mod_id: String, message: String },
    #[error("failed to encode load plan: {0}")]
    Serialization(String),
    #[error("invalid embedded schema: {0}")]
    InvalidSchema(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformErrorCode {
    Io,
    InvalidManifest,
    InvalidPackage,
    PermissionDenied,
    PermissionNotRequested,
    DependencyResolution,
    RuntimeSetup,
    WasmModule,
    Serialization,
    InvalidSchema,
}

impl PlatformErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "platform.io",
            Self::InvalidManifest => "platform.invalid_manifest",
            Self::InvalidPackage => "platform.invalid_package",
            Self::PermissionDenied => "platform.permission_denied",
            Self::PermissionNotRequested => "platform.permission_not_requested",
            Self::DependencyResolution => "platform.dependency_resolution",
            Self::RuntimeSetup => "platform.runtime_setup",
            Self::WasmModule => "platform.wasm_module",
            Self::Serialization => "platform.serialization",
            Self::InvalidSchema => "platform.invalid_schema",
        }
    }
}

impl PlatformError {
    pub const fn code(&self) -> PlatformErrorCode {
        match self {
            Self::Io { .. } => PlatformErrorCode::Io,
            Self::InvalidManifest(_) => PlatformErrorCode::InvalidManifest,
            Self::InvalidPackage(_) => PlatformErrorCode::InvalidPackage,
            Self::PermissionDenied { .. } => PlatformErrorCode::PermissionDenied,
            Self::PermissionNotRequested { .. } => PlatformErrorCode::PermissionNotRequested,
            Self::DependencyResolution(_) => PlatformErrorCode::DependencyResolution,
            Self::RuntimeSetup(_) => PlatformErrorCode::RuntimeSetup,
            Self::WasmModule { .. } => PlatformErrorCode::WasmModule,
            Self::Serialization(_) => PlatformErrorCode::Serialization,
            Self::InvalidSchema(_) => PlatformErrorCode::InvalidSchema,
        }
    }
}

impl ModPackage {
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let root = canonicalize(directory.as_ref())?;
        if !root.is_dir() {
            return Err(PlatformError::InvalidPackage(format!(
                "Mod root `{}` is not a directory",
                root.display()
            )));
        }

        let manifest_path = root.join(MANIFEST_NAME);
        let manifest_path = canonicalize(&manifest_path)?;
        ensure_inside(&root, &manifest_path)?;
        let manifest_size = fs::metadata(&manifest_path)
            .map_err(|error| io_error(&manifest_path, error))?
            .len();
        if manifest_size > MAX_MANIFEST_BYTES {
            return Err(PlatformError::InvalidManifest(format!(
                "manifest exceeds {MAX_MANIFEST_BYTES} bytes"
            )));
        }
        let manifest_bytes =
            fs::read(&manifest_path).map_err(|error| io_error(&manifest_path, error))?;
        let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| PlatformError::InvalidManifest(error.to_string()))?;
        validate_manifest_schema(&manifest_value)?;
        let manifest: ModManifest = serde_json::from_value(manifest_value)
            .map_err(|error| PlatformError::InvalidManifest(error.to_string()))?;
        validate_manifest(&manifest)?;

        validate_entrypoint_path(&manifest.entrypoint).map_err(PlatformError::InvalidManifest)?;
        let relative_entry = Path::new(&manifest.entrypoint);
        let module_path = canonicalize(&root.join(relative_entry))?;
        ensure_inside(&root, &module_path)?;
        if !module_path.is_file() {
            return Err(PlatformError::InvalidPackage(
                "entrypoint does not refer to a file".to_owned(),
            ));
        }
        let module_size = fs::metadata(&module_path)
            .map_err(|error| io_error(&module_path, error))?
            .len();
        if module_size > MAX_MODULE_BYTES {
            return Err(PlatformError::InvalidPackage(format!(
                "module exceeds {MAX_MODULE_BYTES} bytes"
            )));
        }
        let module_bytes = fs::read(&module_path).map_err(|error| io_error(&module_path, error))?;
        let content_digest = package_digest(&manifest, &module_bytes)?;

        Ok(Self {
            root,
            manifest,
            module_bytes,
            content_digest,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub fn discover_mods(directory: impl AsRef<Path>) -> Result<Vec<ModPackage>, PlatformError> {
    let root = canonicalize(directory.as_ref())?;
    if !root.is_dir() {
        return Err(PlatformError::InvalidPackage(format!(
            "Mod directory `{}` is not a directory",
            root.display()
        )));
    }
    let entries = fs::read_dir(&root).map_err(|error| io_error(&root, error))?;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error(&root, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| io_error(&entry.path(), error))?;
        if file_type.is_dir() {
            directories.push(entry.path());
        }
    }
    directories.sort();

    let mut packages = Vec::new();
    for directory in directories {
        if directory.join(MANIFEST_NAME).is_file() {
            packages.push(ModPackage::load(directory)?);
        }
    }
    packages.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    Ok(packages)
}

pub fn resolve_load_plan(packages: &[ModPackage]) -> Result<LoadPlan, PlatformError> {
    let mut by_id = BTreeMap::<String, &ModPackage>::new();
    for package in packages {
        if by_id.insert(package.manifest.id.clone(), package).is_some() {
            return Err(PlatformError::DependencyResolution(format!(
                "duplicate Mod ID `{}`",
                package.manifest.id
            )));
        }
    }

    let mut incoming = by_id
        .keys()
        .map(|id| (id.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut dependents = by_id
        .keys()
        .map(|id| (id.clone(), BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();

    for (id, package) in &by_id {
        let mut seen_dependencies = BTreeSet::new();
        // Manifest array order is user-controlled. Resolve dependencies in a
        // canonical order so that, when several entries are invalid, the
        // selected diagnostic is stable as well as the resulting plan.
        let mut dependencies = package.manifest.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.version.cmp(&right.version))
        });
        for dependency in dependencies {
            if !seen_dependencies.insert(dependency.id.clone()) {
                return Err(PlatformError::DependencyResolution(format!(
                    "Mod `{id}` repeats dependency `{}`",
                    dependency.id
                )));
            }
            let provider = by_id.get(&dependency.id).ok_or_else(|| {
                PlatformError::DependencyResolution(format!(
                    "Mod `{id}` requires missing Mod `{}`",
                    dependency.id
                ))
            })?;
            let requirement = VersionReq::parse(&dependency.version).map_err(|error| {
                PlatformError::DependencyResolution(format!(
                    "Mod `{id}` has invalid version range `{}`: {error}",
                    dependency.version
                ))
            })?;
            let version = Version::parse(&provider.manifest.version).map_err(|error| {
                PlatformError::InvalidManifest(format!("invalid Mod version: {error}"))
            })?;
            if !requirement.matches(&version) {
                return Err(PlatformError::DependencyResolution(format!(
                    "Mod `{id}` requires `{}` {}, found {}",
                    dependency.id, dependency.version, provider.manifest.version
                )));
            }
            *incoming.get_mut(id).expect("all package IDs have indegree") += 1;
            dependents
                .get_mut(&dependency.id)
                .expect("dependency provider exists")
                .insert(id.clone());
        }
    }

    let mut ready = incoming
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(id.clone()))
        .collect::<BTreeSet<_>>();
    let mut ordered_ids = Vec::with_capacity(packages.len());
    while let Some(id) = ready.pop_first() {
        ordered_ids.push(id.clone());
        for dependent in dependents
            .get(&id)
            .expect("all package IDs have dependents")
        {
            let count = incoming.get_mut(dependent).expect("dependent has indegree");
            *count -= 1;
            if *count == 0 {
                ready.insert(dependent.clone());
            }
        }
    }
    if ordered_ids.len() != packages.len() {
        let cycle = incoming
            .into_iter()
            .filter_map(|(id, count)| (count > 0).then_some(id))
            .collect::<Vec<_>>();
        return Err(PlatformError::DependencyResolution(format!(
            "dependency cycle involves: {}",
            cycle.join(" -> ")
        )));
    }

    let mods = ordered_ids
        .iter()
        .map(|id| {
            let package = by_id.get(id).expect("ordered IDs came from package IDs");
            let mut dependencies = package
                .manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.id.clone())
                .collect::<Vec<_>>();
            dependencies.sort();
            LockedMod {
                id: id.clone(),
                version: package.manifest.version.clone(),
                content_digest: package.content_digest.clone(),
                dependencies,
            }
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&(1_u32, KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &mods))
        .map_err(|error| PlatformError::Serialization(error.to_string()))?;
    let config_digest = sha256_hex(&payload);

    Ok(LoadPlan {
        format_version: 1,
        kernel_api: KERNEL_API_VERSION.to_owned(),
        runtime: WASMTIME_RUNTIME_ID.to_owned(),
        mods,
        config_digest,
    })
}

pub fn create_wasm_engine() -> Result<Engine, PlatformError> {
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .max_wasm_stack(MAX_WASM_STACK_BYTES)
        .wasm_simd(false)
        .wasm_features(WasmFeatures::RELAXED_SIMD, false);
    Engine::new(&config).map_err(|error| PlatformError::RuntimeSetup(error.to_string()))
}

pub struct LoadedWasmMod {
    id: String,
    engine: Engine,
    module: Arc<Module>,
    pub content_digest: String,
}

impl LoadedWasmMod {
    pub fn compile(
        package: &ModPackage,
        granted_permissions: &BTreeSet<String>,
        engine: &Engine,
    ) -> Result<Self, PlatformError> {
        for permission in &package.manifest.requested_permissions {
            if !granted_permissions.contains(permission) {
                return Err(PlatformError::PermissionDenied {
                    mod_id: package.manifest.id.clone(),
                    permission: permission.clone(),
                });
            }
        }

        let module = Module::new(engine, &package.module_bytes).map_err(|error| {
            PlatformError::WasmModule {
                mod_id: package.manifest.id.clone(),
                message: error.to_string(),
            }
        })?;
        for import in module.imports() {
            let required_permission = match (import.module(), import.name()) {
                ("gk", "counter_read") => "world.read",
                ("gk", "counter_set") => "world.command",
                _ => {
                    return Err(PlatformError::WasmModule {
                        mod_id: package.manifest.id.clone(),
                        message: format!(
                            "unsupported import `{}::{}`; only the `gk` counter Host API is available",
                            import.module(),
                            import.name()
                        ),
                    });
                }
            };
            if !package
                .manifest
                .requested_permissions
                .contains(required_permission)
            {
                return Err(PlatformError::PermissionNotRequested {
                    mod_id: package.manifest.id.clone(),
                    permission: required_permission.to_owned(),
                });
            }
        }

        // Instantiate once during validation so imports and the entrypoint signature
        // are checked before the Mod is registered with the Kernel.
        validate_module(engine, &module, &package.manifest.id)?;

        Ok(Self {
            id: package.manifest.id.clone(),
            engine: engine.clone(),
            module: Arc::new(module),
            content_digest: package.content_digest.clone(),
        })
    }

    pub fn system(&self) -> WasmSystem {
        WasmSystem {
            id: self.id.clone(),
            engine: self.engine.clone(),
            module: Arc::clone(&self.module),
            content_digest: self.content_digest.clone(),
        }
    }
}

pub struct WasmSystem {
    id: String,
    engine: Engine,
    module: Arc<Module>,
    content_digest: String,
}

impl System for WasmSystem {
    fn id(&self) -> &str {
        &self.id
    }

    fn execute(&self, world: &World, tick: Tick) -> Result<Vec<Command>, SystemError> {
        let current_counter = match world.resource(COUNTER_RESOURCE) {
            None => 0,
            Some(ComponentValue::Integer(value)) => *value,
            Some(_) => {
                return Err(self.system_error(
                    tick,
                    ModFailureCode::InvalidWorldState,
                    "counter Resource must contain an integer",
                ));
            }
        };
        let mut store = new_store(&self.engine, current_counter)
            .map_err(|error| self.system_error(tick, ModFailureCode::RuntimeSetup, error))?;
        let linker = new_linker(&self.engine)
            .map_err(|error| self.system_error(tick, ModFailureCode::RuntimeSetup, error))?;
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|error| {
                self.system_error(tick, ModFailureCode::ExecutionFailed, error.to_string())
            })?;
        let on_tick = instance
            .get_typed_func::<(), ()>(&mut store, "on_tick")
            .map_err(|error| {
                self.system_error(tick, ModFailureCode::ExecutionFailed, error.to_string())
            })?;
        on_tick.call(&mut store, ()).map_err(|error| {
            self.system_error(tick, ModFailureCode::ExecutionFailed, error.to_string())
        })?;

        let state = store.data();
        if state.write_violation {
            return Err(self.system_error(
                tick,
                ModFailureCode::InvalidOutput,
                "Mod attempted more than one counter write in a Tick",
            ));
        }
        Ok(state
            .pending_counter
            .map(|value| {
                vec![Command::SetResource {
                    key: COUNTER_RESOURCE.to_owned(),
                    value: ComponentValue::Integer(value),
                }]
            })
            .unwrap_or_default())
    }
}

impl WasmSystem {
    fn system_error(
        &self,
        tick: Tick,
        code: ModFailureCode,
        message: impl Into<String>,
    ) -> SystemError {
        SystemError::new(&self.id, tick, code.as_str(), message)
            .with_source_digest(self.content_digest.clone())
    }
}

struct HostState {
    current_counter: i64,
    pending_counter: Option<i64>,
    has_written: bool,
    write_violation: bool,
    limits: StoreLimits,
}

fn new_store(engine: &Engine, current_counter: i64) -> Result<Store<HostState>, String> {
    let state = HostState {
        current_counter,
        pending_counter: None,
        has_written: false,
        write_violation: false,
        limits: StoreLimitsBuilder::new()
            .memory_size(MAX_LINEAR_MEMORY_BYTES)
            .instances(1)
            .memories(1)
            .tables(1)
            .table_elements(64)
            .build(),
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    // Fuel counts Wasm instructions; host functions are deliberately bounded and
    // perform one constant-time read or append one command request.
    store
        .set_fuel(FUEL_PER_TICK)
        .map_err(|error| error.to_string())?;
    Ok(store)
}

fn new_linker(engine: &Engine) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap("gk", "counter_read", |caller: Caller<'_, HostState>| {
            caller.data().current_counter
        })
        .map_err(|error| error.to_string())?;
    linker
        .func_wrap(
            "gk",
            "counter_set",
            |mut caller: Caller<'_, HostState>, value: i64| {
                let state = caller.data_mut();
                if state.has_written {
                    state.write_violation = true;
                } else {
                    state.has_written = true;
                    state.pending_counter = Some(value);
                }
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(linker)
}

fn validate_module(engine: &Engine, module: &Module, mod_id: &str) -> Result<(), PlatformError> {
    let mut store = new_store(engine, 0).map_err(|message| PlatformError::WasmModule {
        mod_id: mod_id.to_owned(),
        message,
    })?;
    let linker = new_linker(engine).map_err(|message| PlatformError::WasmModule {
        mod_id: mod_id.to_owned(),
        message,
    })?;
    let instance =
        linker
            .instantiate(&mut store, module)
            .map_err(|error| PlatformError::WasmModule {
                mod_id: mod_id.to_owned(),
                message: error.to_string(),
            })?;
    instance
        .get_typed_func::<(), ()>(&mut store, "on_tick")
        .map_err(|error| PlatformError::WasmModule {
            mod_id: mod_id.to_owned(),
            message: format!("expected export `on_tick: () -> ()`: {error}"),
        })?;
    Ok(())
}

fn validate_manifest(manifest: &ModManifest) -> Result<(), PlatformError> {
    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(PlatformError::InvalidManifest(format!(
            "unsupported manifest_version {}; expected {MANIFEST_VERSION}",
            manifest.manifest_version
        )));
    }
    validate_mod_id(&manifest.id)?;
    Version::parse(&manifest.version).map_err(|error| {
        PlatformError::InvalidManifest(format!("invalid SemVer version: {error}"))
    })?;
    if manifest.kernel_api != KERNEL_API_VERSION {
        return Err(PlatformError::InvalidManifest(format!(
            "kernel_api `{}` is unsupported; this build requires `{KERNEL_API_VERSION}`",
            manifest.kernel_api
        )));
    }
    let allowed = BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]);
    if let Some(permission) = manifest
        .requested_permissions
        .iter()
        .find(|permission| !allowed.contains(*permission))
    {
        return Err(PlatformError::InvalidManifest(format!(
            "unsupported requested permission `{permission}`"
        )));
    }
    for dependency in &manifest.dependencies {
        validate_mod_id(&dependency.id)?;
        VersionReq::parse(&dependency.version).map_err(|error| {
            PlatformError::InvalidManifest(format!(
                "invalid version requirement for `{}`: {error}",
                dependency.id
            ))
        })?;
    }
    Ok(())
}

fn validate_manifest_schema(value: &serde_json::Value) -> Result<(), PlatformError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/mod-manifest-v1.schema.json"))
                .map_err(|error| error.to_string())?;
        jsonschema::validator_for(&schema).map_err(|error| error.to_string())
    });
    let validator = validator
        .as_ref()
        .map_err(|error| PlatformError::InvalidSchema(error.clone()))?;
    if let Err(error) = validator.validate(value) {
        return Err(PlatformError::InvalidManifest(format!(
            "manifest schema violation at `{}`: {error}",
            error.instance_path()
        )));
    }
    Ok(())
}

fn validate_entrypoint_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 240
        || path.is_absolute()
        || value
            .chars()
            .any(|character| matches!(character, '\\' | ':'))
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(
            "entrypoint must be a portable relative path without `.` or `..` components".to_owned(),
        );
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !matches!(extension, "wasm" | "wat") {
        return Err("entrypoint must end in `.wasm` or development-only `.wat`".to_owned());
    }
    Ok(())
}

fn validate_mod_id(id: &str) -> Result<(), PlatformError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id.split('.').all(|segment| {
            !segment.is_empty()
                && segment.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
                })
                && !segment.starts_with('-')
                && !segment.ends_with('-')
        });
    if !valid {
        return Err(PlatformError::InvalidManifest(format!(
            "invalid Mod ID `{id}`; use lowercase ASCII dot-separated segments"
        )));
    }
    Ok(())
}

fn package_digest(manifest: &ModManifest, module_bytes: &[u8]) -> Result<String, PlatformError> {
    let manifest_bytes = serde_json::to_vec(manifest)
        .map_err(|error| PlatformError::Serialization(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(b"game-kernel-mod-package-v1\0");
    hasher.update((manifest_bytes.len() as u64).to_le_bytes());
    hasher.update(manifest_bytes);
    hasher.update((module_bytes.len() as u64).to_le_bytes());
    hasher.update(module_bytes);
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonicalize(path: &Path) -> Result<PathBuf, PlatformError> {
    fs::canonicalize(path).map_err(|error| io_error(path, error))
}

fn ensure_inside(root: &Path, target: &Path) -> Result<(), PlatformError> {
    if !target.starts_with(root) {
        return Err(PlatformError::InvalidPackage(format!(
            "package path `{}` escapes Mod root",
            target.display()
        )));
    }
    Ok(())
}

fn io_error(path: &Path, error: std::io::Error) -> PlatformError {
    PlatformError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_failure_codes_are_stable_and_namespaced() {
        assert_eq!(
            ModFailureCode::RuntimeSetup.as_str(),
            "platform.mod.runtime_setup"
        );
        assert_eq!(
            ModFailureCode::ExecutionFailed.as_str(),
            "platform.mod.execution_failed"
        );
        assert_eq!(
            ModFailureCode::InvalidOutput.as_str(),
            "platform.mod.invalid_output"
        );
        assert_eq!(
            ModFailureCode::InvalidWorldState.as_str(),
            "platform.mod.invalid_world_state"
        );
    }

    fn sample_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mods/p1-counter")
    }

    #[test]
    fn manifest_v1_schema_is_valid_and_accepts_the_sample() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/mod-manifest-v1.schema.json"))
                .unwrap();
        assert!(jsonschema::meta::validate(&schema).is_ok());

        let package = ModPackage::load(sample_root()).unwrap();
        let manifest = serde_json::to_value(&package.manifest).unwrap();
        assert!(validate_manifest_schema(&manifest).is_ok());

        let example_schema =
            fs::read_to_string(sample_root().join("manifest.schema.json")).unwrap();
        assert_eq!(
            example_schema,
            include_str!("../../../schemas/mod-manifest-v1.schema.json")
        );

        let mut invalid = manifest;
        invalid["unknown_field"] = serde_json::Value::Bool(true);
        let error = validate_manifest_schema(&invalid).unwrap_err();
        assert_eq!(error.code().as_str(), "platform.invalid_manifest");
        assert!(error.to_string().contains("unknown_field"));

        let mut duplicate_permissions: serde_json::Value =
            serde_json::to_value(&ModPackage::load(sample_root()).unwrap().manifest).unwrap();
        duplicate_permissions["requested_permissions"] =
            serde_json::json!(["world.read", "world.read"]);
        assert!(validate_manifest_schema(&duplicate_permissions).is_err());
    }

    #[test]
    fn manifest_entrypoint_paths_are_portable_and_confined() {
        assert!(validate_entrypoint_path("module.wasm").is_ok());
        assert!(validate_entrypoint_path("scripts/module.wat").is_ok());
        for invalid in [
            "",
            "/tmp/module.wasm",
            "C:\\mods\\module.wasm",
            "../module.wasm",
            "scripts/../module.wasm",
            "scripts//module.wasm",
            "scripts\\module.wasm",
            "module.dll",
        ] {
            assert!(
                validate_entrypoint_path(invalid).is_err(),
                "accepted entrypoint path `{invalid}`"
            );
        }
    }

    #[test]
    fn manifest_semantic_validation_rejects_invalid_ids_versions_and_permissions() {
        let base = ModPackage::load(sample_root()).unwrap().manifest;

        let mut invalid = base.clone();
        invalid.id = "org..game".to_owned();
        assert_eq!(
            validate_manifest(&invalid).unwrap_err().code().as_str(),
            "platform.invalid_manifest"
        );

        let mut invalid = base.clone();
        invalid.version = "01.0.0".to_owned();
        assert!(validate_manifest(&invalid).is_err());

        let mut invalid = base.clone();
        invalid
            .requested_permissions
            .insert("world.admin".to_owned());
        assert!(validate_manifest(&invalid).is_err());

        let mut invalid = base;
        invalid.dependencies.push(ModDependency {
            id: "org.game.other".to_owned(),
            version: "not a version range".to_owned(),
        });
        assert!(validate_manifest(&invalid).is_err());
    }

    #[test]
    fn discovers_and_validates_the_sample_package() {
        let mods_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mods");
        let packages = discover_mods(mods_root).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].manifest.id, "org.game.p1-counter");
        assert!(packages[0].content_digest.starts_with("sha256:"));
    }

    #[test]
    fn identical_input_creates_identical_load_plan() {
        let package = ModPackage::load(sample_root()).unwrap();
        let first = resolve_load_plan(std::slice::from_ref(&package)).unwrap();
        let second = resolve_load_plan(std::slice::from_ref(&package)).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.mods[0].id, package.manifest.id);
        assert_eq!(first.runtime, WASMTIME_RUNTIME_ID);
    }

    #[test]
    fn missing_required_dependency_is_rejected() {
        let mut package = ModPackage::load(sample_root()).unwrap();
        package.manifest.dependencies.push(ModDependency {
            id: "org.game.missing".to_owned(),
            version: "^1.0.0".to_owned(),
        });
        let result = resolve_load_plan(&[package]);
        assert!(matches!(
            result,
            Err(PlatformError::DependencyResolution(_))
        ));
    }

    #[test]
    fn insufficient_permission_grant_is_rejected() {
        let package = ModPackage::load(sample_root()).unwrap();
        let engine = create_wasm_engine().unwrap();
        let result = LoadedWasmMod::compile(&package, &BTreeSet::new(), &engine);
        assert!(matches!(
            result,
            Err(PlatformError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn imported_host_api_requires_its_declared_permission() {
        let mut package = ModPackage::load(sample_root()).unwrap();
        package.manifest.requested_permissions = BTreeSet::from(["world.read".to_owned()]);
        let grants = BTreeSet::from(["world.read".to_owned()]);
        let engine = create_wasm_engine().unwrap();

        let result = LoadedWasmMod::compile(&package, &grants, &engine);

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("missing requested permission was accepted"),
        };
        assert!(matches!(
            &error,
            PlatformError::PermissionNotRequested { permission, .. }
                if permission == "world.command"
        ));
        assert_eq!(error.code().as_str(), "platform.permission_not_requested");
    }

    #[test]
    fn authorized_wasm_executes_without_mutable_world_access() {
        let package = ModPackage::load(sample_root()).unwrap();
        let grants = BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]);
        let engine = create_wasm_engine().unwrap();
        let loaded = LoadedWasmMod::compile(&package, &grants, &engine).unwrap();
        let mut world = World::new(kernel_core::WorldId(1));
        world
            .seed_resource(COUNTER_RESOURCE, ComponentValue::Integer(4))
            .unwrap();
        let command = loaded.system().execute(&world, Tick(0)).unwrap();
        assert_eq!(
            command,
            vec![Command::SetResource {
                key: COUNTER_RESOURCE.to_owned(),
                value: ComponentValue::Integer(5),
            }]
        );
        assert_eq!(world.resource_i64(COUNTER_RESOURCE), Some(4));
    }

    #[test]
    fn unsupported_host_import_is_rejected_before_registration() {
        let package = ModPackage::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unsupported-import"),
        )
        .unwrap();
        let grants = BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]);
        let engine = create_wasm_engine().unwrap();
        let result = LoadedWasmMod::compile(&package, &grants, &engine);
        assert!(matches!(result, Err(PlatformError::WasmModule { .. })));
    }

    #[test]
    fn over_limit_memory_is_rejected_before_registration() {
        let package = ModPackage::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory-limit"),
        )
        .unwrap();
        let grants = BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]);
        let engine = create_wasm_engine().unwrap();
        let result = LoadedWasmMod::compile(&package, &grants, &engine);
        assert!(matches!(result, Err(PlatformError::WasmModule { .. })));
    }
}
