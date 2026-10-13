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

/// Re-export of the Wasmtime engine type so external consumers and contract
/// tests can name it without depending on the Wasmtime crate directly.
pub use wasmtime::Engine as WasmEngine;

const MANIFEST_NAME: &str = "mod.json";
const MANIFEST_VERSION: u32 = 1;
const KERNEL_API_VERSION: &str = "0.1";
const WASMTIME_RUNTIME_ID: &str = "wasmtime-48.0.5";
/// Version of the in-memory [`LoadPlan`] encoding.
const LOAD_PLAN_FORMAT_VERSION: u32 = 1;
/// Version of the on-disk `mod-lock.json` document format.
const LOCK_FILE_VERSION: u32 = 1;
/// Domain separator for the canonical configuration digest.
const CONFIG_DIGEST_DOMAIN: &[u8] = b"game-kernel-mod-lock-config-v1\0";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
const MAX_LINEAR_MEMORY_BYTES: usize = 64 * 1024;
const FUEL_PER_TICK: u64 = 10_000;
const MAX_WASM_STACK_BYTES: usize = 256 * 1024;
const COUNTER_RESOURCE: &str = "org.game/counter";

/// Version of the exported API catalog document format.
const API_CATALOG_VERSION: u32 = 1;
/// Provider identity recorded for every host-builtin API.
const HOST_API_PROVIDER: &str = "game-platform";
/// Default API version understood by this build. Every registered host API
/// declares this version; the registry rejects any requested version that is
/// not exactly supported rather than exporting `api_version` unverified.
const HOST_API_VERSION: u32 = 1;

/// Direction of a single guest/host function parameter or result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WasmValueKind {
    I32,
    I64,
    F32,
    F64,
    V128,
}

impl WasmValueKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::V128 => "v128",
        }
    }
}

/// Explicit guest signature: ordered parameter kinds and result kinds.
///
/// The current host API subset exposes at most one parameter and at most one
/// result, matching the `func_wrap` closures it must agree with.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostApiSignature {
    pub params: HostApiValueList,
    pub results: HostApiValueList,
}

/// Fixed-capacity list of value kinds (0 or 1 entries for the current subset).
///
/// A fixed shape keeps serialization deterministic and lets the catalog schema
/// constrain the exact number of entries without free-form arrays.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostApiValueList {
    pub len: u8,
    pub kinds: [Option<WasmValueKind>; 1],
}

impl HostApiValueList {
    pub const fn none() -> Self {
        Self {
            len: 0,
            kinds: [None],
        }
    }

    pub const fn one(kind: WasmValueKind) -> Self {
        Self {
            len: 1,
            kinds: [Some(kind)],
        }
    }

    /// Iterates the declared kinds in order.
    pub fn iter(&self) -> impl Iterator<Item = WasmValueKind> + '_ {
        self.kinds.iter().take(self.len as usize).filter_map(|k| *k)
    }

    /// Compares against an actual Wasmtime value-kind sequence.
    fn matches_actual(&self, actual: &[WasmValueKind]) -> bool {
        let declared = self.iter().collect::<Vec<_>>();
        declared == actual
    }
}

/// When during the Mod lifecycle an API may be called.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostApiPhase {
    /// Read-only query of allowed world state.
    WorldRead,
    /// Submission of a world-modifying command request.
    CommandSubmit,
}

impl HostApiPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorldRead => "world-read",
            Self::CommandSubmit => "command-submit",
        }
    }
}

/// Determinism classification of an API call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostApiDeterminism {
    /// Result depends only on the authoritative world state and inputs.
    Deterministic,
}

impl HostApiDeterminism {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
        }
    }
}

/// Stable failure classification for an API call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostApiFailureClass {
    /// A write was rejected in the current Tick because one already happened.
    WriteRejected,
}

impl HostApiFailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WriteRejected => "write-rejected",
        }
    }
}

/// Machine-readable descriptor for one host-builtin API. This is the single
/// source of truth that the module-import validation, the permission allow-set,
/// the Wasm `Linker` registration and the exported catalog all derive from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostApiDescriptor {
    /// Stable API ID, `"<module>::<field>"`.
    pub id: &'static str,
    /// Wasm import module name.
    pub module: &'static str,
    /// Wasm import field name.
    pub field: &'static str,
    /// API version understood by this build.
    pub api_version: u32,
    /// Mod ID of the provider.
    pub provider: &'static str,
    /// Guest-facing signature.
    pub signature: HostApiSignature,
    /// Permission required to call this API.
    pub permission: &'static str,
    /// Lifecycle phase in which the API may be called.
    pub phase: HostApiPhase,
    /// Determinism classification.
    pub determinism: HostApiDeterminism,
    /// Failure classification.
    pub failure: HostApiFailureClass,
    /// Short human-readable summary.
    pub summary: &'static str,
}

/// The complete set of host APIs implemented by this build, in a fixed
/// declaration order that is also the catalog's canonical order.
///
/// Adding an API means adding a descriptor here and a matching `func_wrap`
/// registration; the contract tests enumerate the real `Linker` entries and
/// fail if the two ever diverge.
const HOST_APIS: &[HostApiDescriptor] = &[
    HostApiDescriptor {
        id: "gk::counter_read",
        module: "gk",
        field: "counter_read",
        api_version: HOST_API_VERSION,
        provider: HOST_API_PROVIDER,
        signature: HostApiSignature {
            params: HostApiValueList::none(),
            results: HostApiValueList::one(WasmValueKind::I64),
        },
        permission: "world.read",
        phase: HostApiPhase::WorldRead,
        determinism: HostApiDeterminism::Deterministic,
        failure: HostApiFailureClass::WriteRejected,
        summary: "Read the current authoritative counter value for this Tick",
    },
    HostApiDescriptor {
        id: "gk::counter_set",
        module: "gk",
        field: "counter_set",
        api_version: HOST_API_VERSION,
        provider: HOST_API_PROVIDER,
        signature: HostApiSignature {
            params: HostApiValueList::one(WasmValueKind::I64),
            results: HostApiValueList::none(),
        },
        permission: "world.command",
        phase: HostApiPhase::CommandSubmit,
        determinism: HostApiDeterminism::Deterministic,
        failure: HostApiFailureClass::WriteRejected,
        summary: "Request a counter write; at most one write is accepted per Tick",
    },
];

/// Looks up a registered host API by its Wasm import module and field names.
fn find_host_api(module: &str, field: &str) -> Option<&'static HostApiDescriptor> {
    HOST_APIS
        .iter()
        .find(|api| api.module == module && api.field == field)
}

/// The set of permissions any registered host API requires, in the catalog's
/// declaration order and de-duplicated. Manifest permission validation derives
/// from this rather than a hand-written list.
fn all_host_api_permissions() -> BTreeSet<&'static str> {
    HOST_APIS.iter().map(|api| api.permission).collect()
}

/// Internal consistency check over the registry source itself: no duplicate IDs,
/// every declared API version is the one this build supports, and the import
/// coordinates are unique. Exposed for contract tests; a regression here fails
/// the build rather than silently exporting a wrong catalog.
pub fn validate_registry() -> Result<(), PlatformError> {
    let mut ids = BTreeSet::new();
    let mut imports = BTreeSet::new();
    for api in HOST_APIS {
        if !ids.insert(api.id) {
            return Err(PlatformError::api_catalog(
                ApiCatalogFailure::new(
                    ApiCatalogCode::DuplicateApiId,
                    format!("registry lists API `{}` more than once", api.id),
                )
                .with_detail(api.id.to_owned()),
            ));
        }
        if !imports.insert((api.module, api.field)) {
            return Err(PlatformError::api_catalog(
                ApiCatalogFailure::new(
                    ApiCatalogCode::DuplicateApiId,
                    format!(
                        "registry maps import `{}::{}` more than once",
                        api.module, api.field
                    ),
                )
                .with_detail(api.id.to_owned()),
            ));
        }
        if api.api_version != HOST_API_VERSION {
            return Err(PlatformError::api_catalog(
                ApiCatalogFailure::new(
                    ApiCatalogCode::UnsupportedApiVersion,
                    format!(
                        "API `{}` declares version {}; this build supports {HOST_API_VERSION}",
                        api.id, api.api_version
                    ),
                )
                .with_detail(api.id.to_owned()),
            ));
        }
    }
    Ok(())
}

/// Stable classification for API catalog failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApiCatalogCode {
    #[serde(rename = "platform.api_catalog.unknown_version")]
    UnknownVersion,
    #[serde(rename = "platform.api_catalog.unsupported_api_version")]
    UnsupportedApiVersion,
    #[serde(rename = "platform.api_catalog.duplicate_api_id")]
    DuplicateApiId,
}

impl ApiCatalogCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownVersion => "platform.api_catalog.unknown_version",
            Self::UnsupportedApiVersion => "platform.api_catalog.unsupported_api_version",
            Self::DuplicateApiId => "platform.api_catalog.duplicate_api_id",
        }
    }
}

/// Structured API catalog failure. Diagnostics are human-readable; machines
/// must match on the stable code.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiCatalogFailure {
    pub code: ApiCatalogCode,
    /// Offending API ID or catalog version, when the failure involves one.
    pub detail: Option<String>,
    pub message: String,
}

impl ApiCatalogFailure {
    fn new(code: ApiCatalogCode, message: impl Into<String>) -> Self {
        Self {
            code,
            detail: None,
            message: message.into(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Stable machine-readable code.
    pub const fn code(&self) -> ApiCatalogCode {
        self.code
    }
}

impl std::fmt::Display for ApiCatalogFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Serializable entry of the exported API catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiCatalogEntry {
    pub id: String,
    pub provider: String,
    pub api_version: u32,
    pub summary: String,
    pub params: Vec<WasmValueKind>,
    pub results: Vec<WasmValueKind>,
    pub permission: String,
    pub phase: HostApiPhase,
    pub determinism: HostApiDeterminism,
    pub failure: HostApiFailureClass,
}

/// Machine-readable catalog of every host API implemented by this build
/// (`Mod Platform Specification` §8.4).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiCatalog {
    pub catalog_version: u32,
    pub kernel_api: String,
    pub runtime: String,
    pub apis: Vec<ApiCatalogEntry>,
}

impl ApiCatalog {
    /// Builds the catalog from the registry. Entries are sorted by API ID so
    /// the output is stable regardless of declaration order.
    pub fn from_registry() -> Self {
        let mut apis = HOST_APIS
            .iter()
            .map(|api| ApiCatalogEntry {
                id: api.id.to_owned(),
                provider: api.provider.to_owned(),
                api_version: api.api_version,
                summary: api.summary.to_owned(),
                params: api.signature.params.iter().collect(),
                results: api.signature.results.iter().collect(),
                permission: api.permission.to_owned(),
                phase: api.phase,
                determinism: api.determinism,
                failure: api.failure,
            })
            .collect::<Vec<_>>();
        apis.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            catalog_version: API_CATALOG_VERSION,
            kernel_api: KERNEL_API_VERSION.to_owned(),
            runtime: WASMTIME_RUNTIME_ID.to_owned(),
            apis,
        }
    }

    /// Serializes the catalog to canonical bytes: pretty JSON with a trailing
    /// newline. Deterministic for a given registry.
    pub fn to_bytes(&self) -> Result<Vec<u8>, PlatformError> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| PlatformError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Verifies the catalog's own version and every entry's declared API version
    /// against the versions this build supports.
    ///
    /// This is the acceptance point for "a version that is not supported must be
    /// rejected": the exported `api_version` is a claim, and a claim the build
    /// cannot honour must fail here rather than be exported unverified. Duplicate
    /// API IDs are rejected as well.
    pub fn verify(&self) -> Result<(), PlatformError> {
        if self.catalog_version != API_CATALOG_VERSION {
            return Err(PlatformError::api_catalog(
                ApiCatalogFailure::new(
                    ApiCatalogCode::UnknownVersion,
                    format!(
                        "unsupported catalog_version {}; this build supports {API_CATALOG_VERSION}",
                        self.catalog_version
                    ),
                )
                .with_detail(self.catalog_version.to_string()),
            ));
        }
        let mut seen = BTreeSet::new();
        for entry in &self.apis {
            if !seen.insert(entry.id.as_str()) {
                return Err(PlatformError::api_catalog(
                    ApiCatalogFailure::new(
                        ApiCatalogCode::DuplicateApiId,
                        format!("API `{}` is listed more than once", entry.id),
                    )
                    .with_detail(entry.id.clone()),
                ));
            }
            if entry.api_version != HOST_API_VERSION {
                return Err(PlatformError::api_catalog(
                    ApiCatalogFailure::new(
                        ApiCatalogCode::UnsupportedApiVersion,
                        format!(
                            "API `{}` declares version {}; this build supports {HOST_API_VERSION}",
                            entry.id, entry.api_version
                        ),
                    )
                    .with_detail(entry.id.clone()),
                ));
            }
        }
        Ok(())
    }
}

/// Returns the machine-readable catalog of host APIs implemented by this build.
pub fn api_catalog() -> ApiCatalog {
    ApiCatalog::from_registry()
}

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

/// On-disk `mod-lock.json` document: a versioned, reproducible record of a
/// frozen [`LoadPlan`].
///
/// The file is a strict serialization of a load plan. Reading it re-verifies
/// every recorded fact against the actual packages on disk (content digests and
/// dependency declarations) instead of trusting the file's own contents.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockFile {
    pub lock_version: u32,
    pub kernel_api: String,
    pub runtime: String,
    pub mods: Vec<LockedMod>,
    pub config_digest: String,
}

/// Stable classification for dependency resolution failures. Diagnostics are
/// human-readable and may change; machines must match on these codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DependencyResolutionCode {
    #[serde(rename = "platform.dependency.duplicate_mod_id")]
    DuplicateModId,
    #[serde(rename = "platform.dependency.duplicate_dependency")]
    DuplicateDependency,
    #[serde(rename = "platform.dependency.missing_dependency")]
    MissingDependency,
    #[serde(rename = "platform.dependency.invalid_version_requirement")]
    InvalidVersionRequirement,
    #[serde(rename = "platform.dependency.version_incompatible")]
    VersionIncompatible,
    #[serde(rename = "platform.dependency.cycle")]
    DependencyCycle,
}

impl DependencyResolutionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateModId => "platform.dependency.duplicate_mod_id",
            Self::DuplicateDependency => "platform.dependency.duplicate_dependency",
            Self::MissingDependency => "platform.dependency.missing_dependency",
            Self::InvalidVersionRequirement => "platform.dependency.invalid_version_requirement",
            Self::VersionIncompatible => "platform.dependency.version_incompatible",
            Self::DependencyCycle => "platform.dependency.cycle",
        }
    }
}

/// Structured dependency resolution failure. Carries a stable code plus the
/// Mod/dependency identities the caller needs to act on; `message` is
/// diagnostic text and is not part of the machine protocol.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DependencyResolutionFailure {
    pub code: DependencyResolutionCode,
    /// Declaring Mod for dependency failures, or the duplicated Mod ID.
    pub mod_id: Option<String>,
    /// Related dependency identity, when the failure involves one.
    pub dependency_id: Option<String>,
    /// Declared version requirement, when the failure involves one.
    pub requirement: Option<String>,
    /// Version found in the candidate package, when the failure involves one.
    pub found_version: Option<String>,
    /// Deterministic cycle path `a -> b -> ... -> a`; empty unless the code is
    /// `DependencyCycle`.
    pub cycle: Vec<String>,
    /// Human-readable diagnostic. Not part of the machine protocol.
    pub message: String,
}

impl DependencyResolutionFailure {
    fn new(code: DependencyResolutionCode, message: impl Into<String>) -> Self {
        Self {
            code,
            mod_id: None,
            dependency_id: None,
            requirement: None,
            found_version: None,
            cycle: Vec::new(),
            message: message.into(),
        }
    }

    fn with_mod(mut self, mod_id: impl Into<String>) -> Self {
        self.mod_id = Some(mod_id.into());
        self
    }

    fn with_dependency(mut self, dependency_id: impl Into<String>) -> Self {
        self.dependency_id = Some(dependency_id.into());
        self
    }

    fn with_requirement(mut self, requirement: impl Into<String>) -> Self {
        self.requirement = Some(requirement.into());
        self
    }

    fn with_found_version(mut self, version: impl Into<String>) -> Self {
        self.found_version = Some(version.into());
        self
    }

    fn with_cycle(mut self, cycle: Vec<String>) -> Self {
        self.cycle = cycle;
        self
    }
}

impl std::fmt::Display for DependencyResolutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

/// Stable classification for lock-file failures. Diagnostics are human-readable
/// and may change; machines must match on these codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LockFileCode {
    /// The lock file is not valid JSON or is missing required structure.
    #[serde(rename = "platform.lock.invalid_format")]
    InvalidFormat,
    /// The lock file declares an unsupported `lock_version`.
    #[serde(rename = "platform.lock.unknown_version")]
    UnknownVersion,
    /// `config_digest` recorded in the file does not match a recomputation.
    #[serde(rename = "platform.lock.config_digest_mismatch")]
    ConfigDigestMismatch,
    /// A locked Mod is missing from, or not present in, the supplied packages.
    #[serde(rename = "platform.lock.package_mismatch")]
    PackageMismatch,
    /// A locked Mod's recorded content digest does not match its package.
    #[serde(rename = "platform.lock.content_digest_mismatch")]
    ContentDigestMismatch,
    /// A locked Mod's recorded dependency set does not match its manifest.
    #[serde(rename = "platform.lock.dependency_mismatch")]
    DependencyMismatch,
    /// The locked Mod order differs from the resolver's canonical order.
    #[serde(rename = "platform.lock.order_mismatch")]
    OrderMismatch,
}

impl LockFileCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidFormat => "platform.lock.invalid_format",
            Self::UnknownVersion => "platform.lock.unknown_version",
            Self::ConfigDigestMismatch => "platform.lock.config_digest_mismatch",
            Self::PackageMismatch => "platform.lock.package_mismatch",
            Self::ContentDigestMismatch => "platform.lock.content_digest_mismatch",
            Self::DependencyMismatch => "platform.lock.dependency_mismatch",
            Self::OrderMismatch => "platform.lock.order_mismatch",
        }
    }
}

/// Structured lock-file failure carrying a stable code plus the Mod / field the
/// failure refers to. `message` is diagnostic only and never a protocol field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LockFileFailure {
    pub code: LockFileCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub message: String,
}

impl LockFileFailure {
    fn new(code: LockFileCode, message: impl Into<String>) -> Self {
        Self {
            code,
            mod_id: None,
            detail: None,
            message: message.into(),
        }
    }

    fn with_mod(mut self, mod_id: impl Into<String>) -> Self {
        self.mod_id = Some(mod_id.into());
        self
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl std::fmt::Display for LockFileFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
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
    DependencyResolution(Box<DependencyResolutionFailure>),
    #[error("Wasm runtime setup failed: {0}")]
    RuntimeSetup(String),
    #[error("Wasm module for Mod `{mod_id}` is invalid: {message}")]
    WasmModule { mod_id: String, message: String },
    #[error("Mod `{mod_id}` imports `{api_id}` with signature `{found}`, expected `{expected}`")]
    ApiSignatureMismatch {
        mod_id: String,
        api_id: String,
        expected: String,
        found: String,
    },
    #[error("failed to encode load plan: {0}")]
    Serialization(String),
    #[error("invalid embedded schema: {0}")]
    InvalidSchema(String),
    #[error("lock file failure: {0}")]
    LockFile(Box<LockFileFailure>),
    #[error("API catalog failure: {0}")]
    ApiCatalog(Box<ApiCatalogFailure>),
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
    ApiSignatureMismatch,
    Serialization,
    InvalidSchema,
    LockFile,
    ApiCatalog,
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
            Self::ApiSignatureMismatch => "platform.api_signature_mismatch",
            Self::Serialization => "platform.serialization",
            Self::InvalidSchema => "platform.invalid_schema",
            Self::LockFile => "platform.lock_file",
            Self::ApiCatalog => "platform.api_catalog",
        }
    }
}

impl PlatformError {
    /// Builds a dependency resolution failure. The payload is boxed because the
    /// structured failure is much larger than every other `PlatformError`
    /// variant, and `Result<_, PlatformError>` is returned on hot paths.
    fn dependency(failure: DependencyResolutionFailure) -> Self {
        Self::DependencyResolution(Box::new(failure))
    }

    /// Builds a lock-file failure with the same boxing rationale as
    /// [`PlatformError::dependency`].
    fn lock(failure: LockFileFailure) -> Self {
        Self::LockFile(Box::new(failure))
    }

    /// Builds an API catalog failure with the same boxing rationale as
    /// [`PlatformError::dependency`].
    fn api_catalog(failure: ApiCatalogFailure) -> Self {
        Self::ApiCatalog(Box::new(failure))
    }

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
            Self::ApiSignatureMismatch { .. } => PlatformErrorCode::ApiSignatureMismatch,
            Self::Serialization(_) => PlatformErrorCode::Serialization,
            Self::InvalidSchema(_) => PlatformErrorCode::InvalidSchema,
            Self::LockFile(_) => PlatformErrorCode::LockFile,
            Self::ApiCatalog(_) => PlatformErrorCode::ApiCatalog,
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
        if by_id.contains_key(&package.manifest.id) {
            return Err(PlatformError::dependency(
                DependencyResolutionFailure::new(
                    DependencyResolutionCode::DuplicateModId,
                    format!(
                        "Mod ID `{}` has more than one candidate package; the Platform does not \
                     choose between candidates and requires the caller to select one",
                        package.manifest.id
                    ),
                )
                .with_mod(package.manifest.id.as_str())
                .with_dependency(package.manifest.id.as_str()),
            ));
        }
        by_id.insert(package.manifest.id.clone(), package);
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
                return Err(PlatformError::dependency(
                    DependencyResolutionFailure::new(
                        DependencyResolutionCode::DuplicateDependency,
                        format!(
                            "Mod `{id}` declares dependency `{}` more than once; the required \
                         dependency set must be unambiguous",
                            dependency.id
                        ),
                    )
                    .with_mod(id.as_str())
                    .with_dependency(dependency.id.as_str())
                    .with_requirement(dependency.version.as_str()),
                ));
            }
            let provider =
                by_id.get(&dependency.id).ok_or_else(|| {
                    PlatformError::dependency(DependencyResolutionFailure::new(
                    DependencyResolutionCode::MissingDependency,
                    format!(
                        "Mod `{id}` requires missing required Mod `{}` {}; a missing required \
                         dependency is never treated as optional",
                        dependency.id, dependency.version
                    ),
                )
                .with_mod(id.as_str())
                .with_dependency(dependency.id.as_str())
                .with_requirement(dependency.version.as_str()))
                })?;
            let requirement = VersionReq::parse(&dependency.version).map_err(|error| {
                PlatformError::dependency(
                    DependencyResolutionFailure::new(
                        DependencyResolutionCode::InvalidVersionRequirement,
                        format!(
                            "Mod `{id}` has invalid version range `{}`: {error}",
                            dependency.version
                        ),
                    )
                    .with_mod(id.as_str())
                    .with_dependency(dependency.id.as_str())
                    .with_requirement(dependency.version.as_str()),
                )
            })?;
            let version = Version::parse(&provider.manifest.version).map_err(|error| {
                PlatformError::InvalidManifest(format!(
                    "invalid Mod version for `{}`: {error}",
                    dependency.id
                ))
            })?;
            if !requirement.matches(&version) {
                return Err(PlatformError::dependency(
                    DependencyResolutionFailure::new(
                        DependencyResolutionCode::VersionIncompatible,
                        format!(
                            "Mod `{id}` requires `{}` {}, found {}",
                            dependency.id, dependency.version, provider.manifest.version
                        ),
                    )
                    .with_mod(id.as_str())
                    .with_dependency(dependency.id.as_str())
                    .with_requirement(dependency.version.as_str())
                    .with_found_version(provider.manifest.version.as_str()),
                ));
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
        let unresolved = incoming
            .iter()
            .filter_map(|(id, count)| (*count > 0).then_some(id.clone()))
            .collect::<BTreeSet<_>>();
        let cycle = find_dependency_cycle(&by_id, &unresolved);
        let mut failure = DependencyResolutionFailure::new(
            DependencyResolutionCode::DependencyCycle,
            format!(
                "required dependency graph must be acyclic; cycle path: {}",
                cycle.join(" -> ")
            ),
        )
        .with_cycle(cycle.clone());
        if let Some(first) = cycle.first() {
            failure = failure.with_mod(first.clone());
        }
        return Err(PlatformError::dependency(failure));
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
    let config_digest = canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &mods)?;

    Ok(LoadPlan {
        format_version: LOAD_PLAN_FORMAT_VERSION,
        kernel_api: KERNEL_API_VERSION.to_owned(),
        runtime: WASMTIME_RUNTIME_ID.to_owned(),
        mods,
        config_digest,
    })
}

/// Canonical configuration digest over the exact inputs that define a frozen
/// load plan: the [`LOAD_PLAN_FORMAT_VERSION`], the Kernel API identifier, the
/// runtime identifier, and the ordered Mod list (each Mod contributing its ID,
/// version, content digest and sorted dependency IDs).
///
/// The input is a fixed, length-prefixed encoding so that a change to any of
/// the Kernel API, the runtime, the ordered Mod combination, a Mod version, a
/// Mod's content digest, or a Mod's dependency set produces a different digest.
fn canonical_config_digest(
    kernel_api: &str,
    runtime: &str,
    mods: &[LockedMod],
) -> Result<String, PlatformError> {
    let payload = serde_json::to_vec(&(LOAD_PLAN_FORMAT_VERSION, kernel_api, runtime, mods))
        .map_err(|error| PlatformError::Serialization(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(CONFIG_DIGEST_DOMAIN);
    hasher.update((payload.len() as u64).to_le_bytes());
    hasher.update(&payload);
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

impl LoadPlan {
    /// Converts the in-memory plan into its on-disk lock document. The mapping
    /// is lossless for the fields the lock file records; the locked Mod order
    /// and every recorded digest are preserved verbatim.
    pub fn to_lock_file(&self) -> LockFile {
        LockFile {
            lock_version: LOCK_FILE_VERSION,
            kernel_api: self.kernel_api.clone(),
            runtime: self.runtime.clone(),
            mods: self.mods.clone(),
            config_digest: self.config_digest.clone(),
        }
    }

    /// Recomputes the canonical configuration digest over this plan's current
    /// `kernel_api`, `runtime` and ordered Mod list. Callers rebuilding a plan
    /// from edited parts can use this to re-seal the digest.
    pub fn recompute_config_digest(&self) -> Result<String, PlatformError> {
        canonical_config_digest(&self.kernel_api, &self.runtime, &self.mods)
    }
}

impl LockFile {
    /// Serializes the lock document to canonical bytes: pretty-printed JSON
    /// with a trailing newline. Deterministic for a given [`LoadPlan`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, PlatformError> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| PlatformError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Parses a lock document from raw bytes.
    ///
    /// The document is validated against the embedded v1 schema and then parsed
    /// strictly: unknown fields, malformed JSON and missing required fields are
    /// all rejected. The Rust parser enforces the same structural rules
    /// independently, so a failure to load the embedded schema cannot silently
    /// disable validation. Version support is enforced separately by
    /// [`LockFile::verify`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PlatformError> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::InvalidFormat,
                    "mod-lock.json is not valid JSON",
                )
                .with_detail(error.to_string()),
            )
        })?;
        validate_lock_file_schema(&value)?;
        serde_json::from_value(value).map_err(|error| {
            PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::InvalidFormat,
                    "mod-lock.json is not a valid, strictly-structured lock document",
                )
                .with_detail(error.to_string()),
            )
        })
    }

    /// Writes the lock document to `path` atomically.
    ///
    /// The bytes are first written to a sibling temporary file and then renamed
    /// over the target, so an interrupted or failing write never truncates or
    /// corrupts an existing lock file.
    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<(), PlatformError> {
        let path = path.as_ref();
        let bytes = self.to_bytes()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mod-lock.json");
        let temp_name = format!(".{file_name}.tmp-{}", std::process::id());
        let temp_path = match parent {
            Some(parent) => parent.join(temp_name),
            None => PathBuf::from(temp_name),
        };
        fs::write(&temp_path, &bytes).map_err(|error| io_error(&temp_path, error))?;
        if let Err(error) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(io_error(path, error));
        }
        Ok(())
    }

    /// Reads and parses a lock document from `path`. The file is not modified.
    pub fn read_from(path: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|error| io_error(path, error))?;
        Self::from_bytes(&bytes)
    }

    /// Re-verifies the lock document against the packages it claims to lock and
    /// returns the frozen [`LoadPlan`] on success.
    ///
    /// Verification recomputes every recorded fact rather than trusting the
    /// file: the lock version, the canonical configuration digest, each Mod's
    /// content digest, each Mod's dependency set, and the Mod order are all
    /// checked against the actual packages and the resolver's canonical order.
    pub fn verify(&self, packages: &[ModPackage]) -> Result<LoadPlan, PlatformError> {
        if self.lock_version != LOCK_FILE_VERSION {
            return Err(PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::UnknownVersion,
                    format!(
                        "unsupported lock_version {}; this Platform supports {LOCK_FILE_VERSION}",
                        self.lock_version
                    ),
                )
                .with_detail(self.lock_version.to_string()),
            ));
        }
        if self.kernel_api != KERNEL_API_VERSION {
            return Err(PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::ConfigDigestMismatch,
                    format!(
                        "lock file targets Kernel API `{}`, this Platform provides `{KERNEL_API_VERSION}`",
                        self.kernel_api
                    ),
                )
                .with_detail(self.kernel_api.clone()),
            ));
        }
        if self.runtime != WASMTIME_RUNTIME_ID {
            return Err(PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::ConfigDigestMismatch,
                    format!(
                        "lock file targets runtime `{}`, this Platform provides `{WASMTIME_RUNTIME_ID}`",
                        self.runtime
                    ),
                )
                .with_detail(self.runtime.clone()),
            ));
        }

        // Index the actual packages so every recorded fact can be re-derived
        // from source rather than trusted from the file.
        let package_by_id = packages
            .iter()
            .map(|package| (package.manifest.id.as_str(), package))
            .collect::<BTreeMap<_, _>>();

        // 1. Each locked Mod must correspond to an actual package whose
        //    version, content digest and dependency set all agree. A duplicate
        //    Mod ID in the lock file is itself a structural violation.
        let mut seen_ids = BTreeSet::new();
        for locked in &self.mods {
            if !seen_ids.insert(locked.id.as_str()) {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::PackageMismatch,
                        format!("lock file lists Mod `{}` more than once", locked.id),
                    )
                    .with_mod(locked.id.as_str()),
                ));
            }
            let Some(package) = package_by_id.get(locked.id.as_str()) else {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::PackageMismatch,
                        format!("lock file references unknown Mod `{}`", locked.id),
                    )
                    .with_mod(locked.id.as_str()),
                ));
            };
            if locked.version != package.manifest.version {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::PackageMismatch,
                        format!(
                            "locked version `{}` for Mod `{}` does not match package version `{}`",
                            locked.version, locked.id, package.manifest.version
                        ),
                    )
                    .with_mod(locked.id.as_str())
                    .with_detail(locked.version.clone()),
                ));
            }
            if locked.content_digest != package.content_digest {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::ContentDigestMismatch,
                        format!(
                            "locked content digest for Mod `{}` does not match the package",
                            locked.id
                        ),
                    )
                    .with_mod(locked.id.as_str())
                    .with_detail(format!(
                        "recorded={} package={}",
                        locked.content_digest, package.content_digest
                    )),
                ));
            }
            // Compare the recorded dependency set against the package's real
            // manifest, not against another derived structure.
            let mut actual_dependencies = package
                .manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.id.clone())
                .collect::<Vec<_>>();
            actual_dependencies.sort();
            if locked.dependencies != actual_dependencies {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::DependencyMismatch,
                        format!(
                            "locked dependency set for Mod `{}` does not match its manifest",
                            locked.id
                        ),
                    )
                    .with_mod(locked.id.as_str())
                    .with_detail(format!(
                        "recorded={:?} manifest={:?}",
                        locked.dependencies, actual_dependencies
                    )),
                ));
            }
        }

        // 2. Recompute the full plan from the packages so that membership,
        //    canonical order and the configuration digest are all derived from
        //    the resolver rather than the file.
        let recomputed = resolve_load_plan(packages)?;

        // 3. Every resolved Mod must be recorded in the file.
        for locked in &recomputed.mods {
            if !seen_ids.contains(locked.id.as_str()) {
                return Err(PlatformError::lock(
                    LockFileFailure::new(
                        LockFileCode::PackageMismatch,
                        format!("lock file is missing resolved Mod `{}`", locked.id),
                    )
                    .with_mod(locked.id.as_str()),
                ));
            }
        }

        // 4. The recorded order must equal the resolver's canonical order.
        let recorded_order = self
            .mods
            .iter()
            .map(|locked| &locked.id)
            .collect::<Vec<_>>();
        let expected_order = recomputed
            .mods
            .iter()
            .map(|locked| &locked.id)
            .collect::<Vec<_>>();
        if recorded_order != expected_order {
            return Err(PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::OrderMismatch,
                    "locked Mod order does not match the resolver's canonical order",
                )
                .with_detail(format!(
                    "recorded={recorded_order:?} expected={expected_order:?}"
                )),
            ));
        }

        // 5. Finally require the recorded configuration digest to match the
        //    digest recomputed over the actual packages.
        if self.config_digest != recomputed.config_digest {
            return Err(PlatformError::lock(
                LockFileFailure::new(
                    LockFileCode::ConfigDigestMismatch,
                    "recorded config_digest does not match the packages on disk",
                )
                .with_detail(format!(
                    "recorded={} recomputed={}",
                    self.config_digest, recomputed.config_digest
                )),
            ));
        }

        Ok(recomputed)
    }
}

/// Returns a deterministic cycle path `a -> b -> ... -> a` inside the set of
/// Mod IDs that Kahn's algorithm could not order.
///
/// Every unresolved Mod still has at least one unresolved required dependency,
/// so repeatedly following the lowest-sorted unresolved dependency must
/// terminate at an already-visited Mod. The result therefore depends only on
/// the package contents, not on the candidate input order.
fn find_dependency_cycle(
    by_id: &BTreeMap<String, &ModPackage>,
    unresolved: &BTreeSet<String>,
) -> Vec<String> {
    let Some(start) = unresolved.first().cloned() else {
        return Vec::new();
    };
    let mut path = vec![start.clone()];
    let mut current = start;
    loop {
        let next = by_id.get(&current).and_then(|package| {
            package
                .manifest
                .dependencies
                .iter()
                .map(|dependency| dependency.id.clone())
                .filter(|id| unresolved.contains(id))
                .min()
        });
        let Some(next) = next else {
            // Defensive: an unresolved Mod always keeps an unresolved
            // dependency, so a reachable cycle exists for every real input.
            return path;
        };
        if let Some(position) = path.iter().position(|id| *id == next) {
            let mut cycle = path.split_off(position);
            cycle.push(next);
            return cycle;
        }
        path.push(next.clone());
        current = next;
    }
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
            let Some(api) = find_host_api(import.module(), import.name()) else {
                return Err(PlatformError::WasmModule {
                    mod_id: package.manifest.id.clone(),
                    message: format!(
                        "unsupported import `{}::{}`; only the `gk` counter Host API is available",
                        import.module(),
                        import.name()
                    ),
                });
            };
            // The registered signature is part of the contract: an import with
            // the right name but the wrong guest signature is rejected before
            // the Mod ever runs.
            if !host_api_signature_matches(api, &import) {
                return Err(PlatformError::ApiSignatureMismatch {
                    mod_id: package.manifest.id.clone(),
                    api_id: api.id.to_owned(),
                    expected: describe_signature(&api.signature),
                    found: describe_import_signature(&import),
                });
            }
            if !package
                .manifest
                .requested_permissions
                .contains(api.permission)
            {
                return Err(PlatformError::PermissionNotRequested {
                    mod_id: package.manifest.id.clone(),
                    permission: api.permission.to_owned(),
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

/// Maps an actual Wasmtime import type to the registry's value-kind domain.
///
/// Returns `None` for any Wasm type the registry cannot express in the current
/// subset (references, etc.), which is treated as a signature mismatch.
fn wasm_value_kind(value: &wasmtime::ValType) -> Option<WasmValueKind> {
    match value {
        wasmtime::ValType::I32 => Some(WasmValueKind::I32),
        wasmtime::ValType::I64 => Some(WasmValueKind::I64),
        wasmtime::ValType::F32 => Some(WasmValueKind::F32),
        wasmtime::ValType::F64 => Some(WasmValueKind::F64),
        wasmtime::ValType::V128 => Some(WasmValueKind::V128),
        wasmtime::ValType::Ref(_) => None,
    }
}

/// Extracts the value kinds of an import's actual `FuncType`, or `None` if the
/// import is not a function or uses an unsupported Wasm type.
fn import_value_kinds(
    import: &wasmtime::ImportType<'_>,
) -> Option<(Vec<WasmValueKind>, Vec<WasmValueKind>)> {
    let extern_type = import.ty();
    let func = extern_type.func()?;
    let params = func
        .params()
        .map(|ty| wasm_value_kind(&ty))
        .collect::<Option<Vec<_>>>()?;
    let results = func
        .results()
        .map(|ty| wasm_value_kind(&ty))
        .collect::<Option<Vec<_>>>()?;
    Some((params, results))
}

/// True when a registry descriptor's declared signature equals the import's
/// actual Wasm signature.
fn host_api_signature_matches(api: &HostApiDescriptor, import: &wasmtime::ImportType<'_>) -> bool {
    match import_value_kinds(import) {
        Some((params, results)) => {
            api.signature.params.matches_actual(&params)
                && api.signature.results.matches_actual(&results)
        }
        None => false,
    }
}

/// Renders a registry signature as `params -> results`.
fn describe_signature(signature: &HostApiSignature) -> String {
    let params = signature
        .params
        .iter()
        .map(WasmValueKind::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let results = signature
        .results
        .iter()
        .map(WasmValueKind::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("({params}) -> ({results})")
}

/// Renders an import's actual signature as `params -> results`.
fn describe_import_signature(import: &wasmtime::ImportType<'_>) -> String {
    match import_value_kinds(import) {
        Some((params, results)) => {
            let params = params
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let results = results
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("({params}) -> ({results})")
        }
        None => "non-function import or unsupported value type".to_owned(),
    }
}

/// Builds the Wasm `Linker` for host API calls.
///
/// Every `func_wrap` here must correspond to exactly one [`HostApiDescriptor`]
/// in [`HOST_APIS`]: the contract tests enumerate this linker's entries and
/// their real signatures and fail if the two sets diverge. The registrations
/// are kept explicit rather than driven by a dynamic loop because each closure
/// has a distinct Rust type; the test, not the runtime, enforces sameness.
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

/// Test-only introspection of the **real** Wasm `Linker` built by the Platform.
///
/// Enumerates `Linker::iter`, returning `(module, field, rendered_signature)`
/// for every registered function. This lets contract tests compare the runtime
/// allow-list against the Registry/Catalog without a hand-written key list.
pub fn linker_registration_probe() -> Result<Vec<(String, String, String)>, String> {
    let engine = create_wasm_engine().map_err(|error| error.to_string())?;
    let mut store = new_store(&engine, 0)?;
    let linker = new_linker(&engine)?;
    // Materialize the raw entries first so the mutable borrow of `store` used by
    // `Linker::iter` ends before each item's type is queried.
    let raw = linker
        .iter(&mut store)
        .map(|(module, field, item)| (module.to_owned(), field.to_owned(), item))
        .collect::<Vec<_>>();
    let mut entries = Vec::new();
    for (module, field, item) in raw {
        let signature = match item.ty(&store) {
            wasmtime::ExternType::Func(func) => {
                let params = func
                    .params()
                    .map(|ty| {
                        wasm_value_kind(&ty)
                            .map_or_else(|| "?".to_owned(), |k| k.as_str().to_owned())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let results = func
                    .results()
                    .map(|ty| {
                        wasm_value_kind(&ty)
                            .map_or_else(|| "?".to_owned(), |k| k.as_str().to_owned())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({params}) -> ({results})")
            }
            other => format!("{other:?}"),
        };
        entries.push((module, field, signature));
    }
    entries.sort();
    Ok(entries)
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
    let allowed = all_host_api_permissions();
    if let Some(permission) = manifest
        .requested_permissions
        .iter()
        .find(|permission| !allowed.contains(permission.as_str()))
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

fn validate_lock_file_schema(value: &serde_json::Value) -> Result<(), PlatformError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/mod-lock-v1.schema.json"))
                .map_err(|error| error.to_string())?;
        jsonschema::validator_for(&schema).map_err(|error| error.to_string())
    });
    let validator = validator
        .as_ref()
        .map_err(|error| PlatformError::InvalidSchema(error.clone()))?;
    if let Err(error) = validator.validate(value) {
        return Err(PlatformError::lock(
            LockFileFailure::new(
                LockFileCode::InvalidFormat,
                "mod-lock.json violates the v1 lock schema",
            )
            .with_detail(format!(
                "schema violation at `{}`: {error}",
                error.instance_path()
            )),
        ));
    }
    Ok(())
}

/// Validates a serialized catalog value against the embedded v1 catalog schema.
///
/// Exposed for contract tests so they can prove the exported bytes satisfy the
/// published schema; the Platform itself builds the catalog from code.
pub fn validate_api_catalog_value(value: &serde_json::Value) -> Result<(), String> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../schemas/mod-api-catalog-v1.schema.json"
        ))
        .map_err(|error| error.to_string())?;
        jsonschema::validator_for(&schema).map_err(|error| error.to_string())
    });
    let validator = validator.as_ref().map_err(|error| error.clone())?;
    if let Err(error) = validator.validate(value) {
        return Err(format!(
            "api catalog schema violation at `{}`: {error}",
            error.instance_path()
        ));
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
    fn registry_is_internally_consistent() {
        validate_registry().expect("the built-in registry must be self-consistent");
    }

    #[test]
    fn api_catalog_is_sorted_stable_and_verifies() {
        let catalog = api_catalog();
        catalog.verify().expect("built catalog must verify");
        let ids = catalog
            .apis
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "catalog entries must be sorted by API ID");
        assert_eq!(
            catalog.to_bytes().unwrap(),
            catalog.to_bytes().unwrap(),
            "catalog serialization must be deterministic"
        );
    }

    #[test]
    fn api_catalog_only_exports_implemented_apis() {
        let catalog = api_catalog();
        // Exactly the two host APIs this build implements; nothing implied by
        // the wider specification may appear here.
        assert_eq!(catalog.apis.len(), 2);
        assert_eq!(catalog.apis[0].id, "gk::counter_read");
        assert_eq!(catalog.apis[1].id, "gk::counter_set");
    }

    #[test]
    fn api_catalog_rejects_unsupported_version() {
        let mut catalog = api_catalog();
        catalog.catalog_version = 999;
        assert_eq!(
            catalog.verify().unwrap_err().code(),
            PlatformErrorCode::ApiCatalog
        );
        let mut catalog = api_catalog();
        catalog.apis[0].api_version = 999;
        assert_eq!(
            catalog.verify().unwrap_err().code(),
            PlatformErrorCode::ApiCatalog
        );
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
    fn lock_v1_schema_is_valid_and_matches_the_serialized_plan() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/mod-lock-v1.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(jsonschema::meta::validate(&schema).is_ok());

        let package = ModPackage::load(sample_root()).unwrap();
        let plan = resolve_load_plan(std::slice::from_ref(&package)).unwrap();
        let lock_value = serde_json::to_value(plan.to_lock_file()).unwrap();
        assert!(validator.validate(&lock_value).is_ok());

        // The Rust parser must reject the same unknown-field case the schema
        // rejects, independently of schema availability.
        let mut unknown = lock_value.clone();
        unknown["unexpected"] = serde_json::Value::Bool(true);
        assert!(matches!(
            LockFile::from_bytes(&serde_json::to_vec(&unknown).unwrap()),
            Err(PlatformError::LockFile(_))
        ));
    }

    #[test]
    fn config_digest_changes_with_every_locked_input() {
        let base = LockedMod {
            id: "org.game.a".to_owned(),
            version: "1.0.0".to_owned(),
            content_digest: "sha256:aa".to_owned(),
            dependencies: vec!["org.game.b".to_owned()],
        };
        let baseline = canonical_config_digest(
            KERNEL_API_VERSION,
            WASMTIME_RUNTIME_ID,
            std::slice::from_ref(&base),
        )
        .unwrap();

        let mut changed_content = base.clone();
        changed_content.content_digest = "sha256:bb".to_owned();
        let mut changed_version = base.clone();
        changed_version.version = "2.0.0".to_owned();
        let mut changed_deps = base.clone();
        changed_deps.dependencies = vec!["org.game.c".to_owned()];

        let cases = [
            canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &[changed_content])
                .unwrap(),
            canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &[changed_version])
                .unwrap(),
            canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &[changed_deps])
                .unwrap(),
            canonical_config_digest("9.9", WASMTIME_RUNTIME_ID, std::slice::from_ref(&base))
                .unwrap(),
            canonical_config_digest(KERNEL_API_VERSION, "runtime-x", std::slice::from_ref(&base))
                .unwrap(),
            canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &[]).unwrap(),
        ];
        for digest in cases {
            assert_ne!(digest, baseline, "a changed input must change the digest");
        }
        // Recomputing the same input is stable.
        assert_eq!(
            canonical_config_digest(KERNEL_API_VERSION, WASMTIME_RUNTIME_ID, &[base]).unwrap(),
            baseline
        );
    }

    #[test]
    fn missing_required_dependency_is_rejected() {
        let mut package = ModPackage::load(sample_root()).unwrap();
        package.manifest.dependencies.push(ModDependency {
            id: "org.game.missing".to_owned(),
            version: "^1.0.0".to_owned(),
        });
        let failure = dependency_failure(resolve_load_plan(&[package]));
        assert_eq!(failure.code, DependencyResolutionCode::MissingDependency);
        assert_eq!(
            failure.code.as_str(),
            "platform.dependency.missing_dependency"
        );
        assert_eq!(failure.mod_id.as_deref(), Some("org.game.p1-counter"));
        assert_eq!(failure.dependency_id.as_deref(), Some("org.game.missing"));
        assert_eq!(failure.requirement.as_deref(), Some("^1.0.0"));
    }

    /// Synthetic packages keep the resolver tests focused on the public
    /// `resolve_load_plan` contract without needing a directory on disk.
    fn synthetic_package(id: &str, version: &str, dependencies: &[(&str, &str)]) -> ModPackage {
        ModPackage {
            root: PathBuf::from(format!("synthetic/{id}")),
            manifest: ModManifest {
                manifest_version: MANIFEST_VERSION,
                id: id.to_owned(),
                version: version.to_owned(),
                kernel_api: KERNEL_API_VERSION.to_owned(),
                entrypoint: "module.wat".to_owned(),
                requested_permissions: BTreeSet::new(),
                dependencies: dependencies
                    .iter()
                    .map(|(id, version)| ModDependency {
                        id: (*id).to_owned(),
                        version: (*version).to_owned(),
                    })
                    .collect(),
            },
            module_bytes: Vec::new(),
            content_digest: format!("sha256:synthetic-{id}-{version}"),
        }
    }

    fn dependency_failure(result: Result<LoadPlan, PlatformError>) -> DependencyResolutionFailure {
        match result {
            Err(PlatformError::DependencyResolution(failure)) => *failure,
            other => panic!("expected a dependency resolution failure, got {other:?}"),
        }
    }

    /// Every ordering of the candidate slice, so that "all optional input
    /// permutations produce the same plan" is actually exercised.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut result = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(index);
            for tail in permutations(&rest) {
                let mut permutation = vec![item.clone()];
                permutation.extend(tail);
                result.push(permutation);
            }
        }
        result
    }

    fn diamond_graph() -> Vec<ModPackage> {
        vec![
            synthetic_package("org.game.core", "0.1.0", &[]),
            synthetic_package("org.game.lib-a", "0.2.0", &[("org.game.core", "^0.1.0")]),
            synthetic_package("org.game.lib-b", "0.2.0", &[("org.game.core", "^0.1.0")]),
            synthetic_package(
                "org.game.app",
                "1.0.0",
                // Deliberately unsorted so the resolver must canonicalize.
                &[("org.game.lib-b", "^0.2.0"), ("org.game.lib-a", "^0.2.0")],
            ),
        ]
    }

    #[test]
    fn every_candidate_order_produces_the_same_load_plan() {
        let packages = diamond_graph();
        let expected = resolve_load_plan(&packages).unwrap();
        assert_eq!(
            expected
                .mods
                .iter()
                .map(|locked| locked.id.as_str())
                .collect::<Vec<_>>(),
            [
                "org.game.core",
                "org.game.lib-a",
                "org.game.lib-b",
                "org.game.app"
            ]
        );
        let app = expected
            .mods
            .iter()
            .find(|locked| locked.id == "org.game.app")
            .unwrap();
        assert_eq!(
            app.dependencies,
            ["org.game.lib-a", "org.game.lib-b"],
            "locked dependency lists must be canonical, not manifest order"
        );

        let orderings = permutations(&packages);
        assert_eq!(orderings.len(), 24);
        for ordering in orderings {
            assert_eq!(resolve_load_plan(&ordering).unwrap(), expected);
        }
    }

    #[test]
    fn repeated_resolution_is_stable() {
        let packages = diamond_graph();
        let first = resolve_load_plan(&packages).unwrap();
        for _ in 0..5 {
            assert_eq!(resolve_load_plan(&packages).unwrap(), first);
        }
        assert!(first.config_digest.starts_with("sha256:"));
    }

    #[test]
    fn duplicate_mod_ids_are_rejected_instead_of_silently_chosen() {
        let first = synthetic_package("org.game.dup", "1.0.0", &[]);
        let second = synthetic_package("org.game.dup", "1.0.1", &[]);
        let failure = dependency_failure(resolve_load_plan(&[first.clone(), second.clone()]));
        assert_eq!(failure.code, DependencyResolutionCode::DuplicateModId);
        assert_eq!(
            failure.code.as_str(),
            "platform.dependency.duplicate_mod_id"
        );
        assert_eq!(failure.mod_id.as_deref(), Some("org.game.dup"));
        assert!(failure.message.contains("does not"));

        // Candidate order must not change which diagnostic is reported.
        assert_eq!(
            dependency_failure(resolve_load_plan(&[second, first])),
            failure
        );
    }

    #[test]
    fn duplicate_dependency_declarations_are_rejected() {
        let packages = vec![
            synthetic_package("org.game.core", "0.1.0", &[]),
            synthetic_package(
                "org.game.app",
                "1.0.0",
                &[("org.game.core", "^0.1.0"), ("org.game.core", "^0.1.0")],
            ),
        ];
        let failure = dependency_failure(resolve_load_plan(&packages));
        assert_eq!(failure.code, DependencyResolutionCode::DuplicateDependency);
        assert_eq!(
            failure.code.as_str(),
            "platform.dependency.duplicate_dependency"
        );
        assert_eq!(failure.mod_id.as_deref(), Some("org.game.app"));
        assert_eq!(failure.dependency_id.as_deref(), Some("org.game.core"));
    }

    #[test]
    fn version_mismatch_reports_the_requirement_and_the_found_version() {
        let packages = vec![
            synthetic_package("org.game.core", "1.0.0", &[]),
            synthetic_package("org.game.app", "1.0.0", &[("org.game.core", "^2.0.0")]),
        ];
        let failure = dependency_failure(resolve_load_plan(&packages));
        assert_eq!(failure.code, DependencyResolutionCode::VersionIncompatible);
        assert_eq!(
            failure.code.as_str(),
            "platform.dependency.version_incompatible"
        );
        assert_eq!(failure.mod_id.as_deref(), Some("org.game.app"));
        assert_eq!(failure.dependency_id.as_deref(), Some("org.game.core"));
        assert_eq!(failure.requirement.as_deref(), Some("^2.0.0"));
        assert_eq!(failure.found_version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn invalid_version_requirement_is_a_structured_resolver_failure() {
        let packages = vec![
            synthetic_package("org.game.core", "1.0.0", &[]),
            synthetic_package("org.game.app", "1.0.0", &[("org.game.core", "not a range")]),
        ];
        let failure = dependency_failure(resolve_load_plan(&packages));
        assert_eq!(
            failure.code,
            DependencyResolutionCode::InvalidVersionRequirement
        );
        assert_eq!(failure.requirement.as_deref(), Some("not a range"));
    }

    #[test]
    fn self_dependency_is_reported_as_a_cycle() {
        let packages = vec![synthetic_package(
            "org.game.loop",
            "1.0.0",
            &[("org.game.loop", "^1.0.0")],
        )];
        let failure = dependency_failure(resolve_load_plan(&packages));
        assert_eq!(failure.code, DependencyResolutionCode::DependencyCycle);
        assert_eq!(failure.cycle, ["org.game.loop", "org.game.loop"]);
        assert_eq!(failure.mod_id.as_deref(), Some("org.game.loop"));
    }

    #[test]
    fn multi_node_cycle_reports_a_stable_full_path_for_every_input_order() {
        let packages = vec![
            synthetic_package("org.game.a", "1.0.0", &[("org.game.b", "^1.0.0")]),
            synthetic_package("org.game.b", "1.0.0", &[("org.game.c", "^1.0.0")]),
            synthetic_package("org.game.c", "1.0.0", &[("org.game.a", "^1.0.0")]),
        ];
        let expected = ["org.game.a", "org.game.b", "org.game.c", "org.game.a"];
        let orderings = permutations(&packages);
        assert_eq!(orderings.len(), 6);
        for ordering in orderings {
            let failure = dependency_failure(resolve_load_plan(&ordering));
            assert_eq!(failure.code, DependencyResolutionCode::DependencyCycle);
            assert_eq!(failure.code.as_str(), "platform.dependency.cycle");
            assert_eq!(failure.cycle, expected);
            assert!(failure.message.contains("org.game.a -> org.game.b"));
        }
    }

    #[test]
    fn cyclic_subgraph_does_not_hide_an_otherwise_orderable_graph() {
        let packages = vec![
            synthetic_package("org.game.ok", "1.0.0", &[]),
            synthetic_package("org.game.x", "1.0.0", &[("org.game.y", "^1.0.0")]),
            synthetic_package("org.game.y", "1.0.0", &[("org.game.x", "^1.0.0")]),
        ];
        let failure = dependency_failure(resolve_load_plan(&packages));
        assert_eq!(failure.code, DependencyResolutionCode::DependencyCycle);
        assert_eq!(failure.cycle, ["org.game.x", "org.game.y", "org.game.x"]);
        assert!(!failure.cycle.iter().any(|id| id == "org.game.ok"));
    }

    #[test]
    fn missing_and_version_failures_are_reported_without_guessing_dependency_order() {
        // `org.game.app` has two broken required dependencies. The selected
        // diagnostic must be stable regardless of manifest array order.
        let forward = vec![
            synthetic_package("org.game.core", "1.0.0", &[]),
            synthetic_package(
                "org.game.app",
                "1.0.0",
                &[("org.game.absent", "^1.0.0"), ("org.game.core", "^2.0.0")],
            ),
        ];
        let mut reversed_manifest = forward.clone();
        reversed_manifest[1].manifest.dependencies.reverse();
        let expected = dependency_failure(resolve_load_plan(&forward));
        assert_eq!(
            dependency_failure(resolve_load_plan(&reversed_manifest)),
            expected
        );
        // "org.game.absent" sorts before "org.game.core", so it is reported first.
        assert_eq!(expected.code, DependencyResolutionCode::MissingDependency);
        assert_eq!(expected.dependency_id.as_deref(), Some("org.game.absent"));
    }

    #[test]
    fn resolver_failure_serializes_with_stable_protocol_fields() {
        let packages = vec![
            synthetic_package("org.game.core", "1.0.0", &[]),
            synthetic_package("org.game.app", "1.0.0", &[("org.game.core", "^2.0.0")]),
        ];
        let failure = dependency_failure(resolve_load_plan(&packages));
        let report = serde_json::to_value(&failure).unwrap();
        assert_eq!(report["code"], "platform.dependency.version_incompatible");
        assert_eq!(report["mod_id"], "org.game.app");
        assert_eq!(report["dependency_id"], "org.game.core");
        assert_eq!(report["requirement"], "^2.0.0");
        assert_eq!(report["found_version"], "1.0.0");
        assert_eq!(report["cycle"], serde_json::json!([]));
        assert_eq!(
            PlatformError::DependencyResolution(Box::new(failure))
                .code()
                .as_str(),
            "platform.dependency_resolution"
        );
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
