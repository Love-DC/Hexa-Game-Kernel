//! Platform layer: package discovery, strict manifest validation, deterministic
//! dependency locking, and a restricted Wasm-to-Kernel adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use kernel_core::{Command, ComponentValue, Kernel, System, SystemError, Tick, World};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::{
    Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Trap,
    WasmFeatures,
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

/// Stable failure codes for a Mod's Wasm execution, as surfaced through the
/// Kernel `SystemError.code` field.
///
/// GK-P3-06 refines the previously blanket `ExecutionFailed` mapping: a runtime
/// failure now reports its frozen §3.2/§3.3 stable code. `ExecutionFailed`
/// remains as the conservative fallback for a runtime failure whose trap cannot
/// be classified. The message is never consulted for classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModFailureCode {
    RuntimeSetup,
    /// Conservative fallback: a runtime failure not otherwise classified.
    ExecutionFailed,
    InvalidOutput,
    InvalidWorldState,
    /// Frozen §3.3 C.1: runtime fuel exhaustion (`Trap::OutOfFuel`).
    FuelExceeded,
    /// Frozen §3.3 C.1: runtime linear-memory out of bounds (`Trap::MemoryOutOfBounds`).
    MemoryOutOfBounds,
    /// Frozen §3.3 C.1: runtime stack exhaustion (`Trap::StackOverflow`).
    StackExceeded,
    /// Frozen §3.3 C.1: resource quota exceeded (commands/events/tasks; future).
    QuotaExceeded,
    /// Frozen §3.2: guest `unreachable` (`Trap::UnreachableCodeReached`).
    GuestTrapUnreachable,
    /// Frozen §3.2: guest integer overflow (`Trap::IntegerOverflow`).
    GuestTrapIntegerOverflow,
    /// Frozen §3.2: guest integer divide by zero (`Trap::IntegerDivisionByZero`).
    GuestTrapIntegerDivideByZero,
    /// Frozen §3.2: guest bad conversion to integer (`Trap::BadConversionToInteger`).
    GuestTrapInvalidConversion,
    /// Frozen §3.2: guest interrupt (`Trap::Interrupt`); the only retryable trap.
    GuestTrapInterrupted,
    /// Frozen §3.2: any other enabled trap variant (`other` group).
    GuestTrapOther,
    /// Frozen §3.2: a `#[non_exhaustive]` trap variant unknown to this build.
    GuestTrapUnknown,
    /// Frozen §3.2/D: host API invocation failure.
    ApiInvocationFailed,
    /// Frozen §3.3 C.2: instantiation-phase linear-memory limit exceeded.
    MemoryLimitExceeded,
    /// Frozen §3.3 C.2: instantiation-phase Wasm stack limit exceeded.
    StackLimitExceeded,
    /// Frozen §3 row 6 (LoadError): module failed to load/instantiate.
    LoadFailed,
    /// Frozen §3 row 7 (InitializationError): Mod initialization failed.
    InitializationFailed,
}

impl ModFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeSetup => "platform.mod.runtime_setup",
            Self::ExecutionFailed => "platform.mod.execution_failed",
            Self::InvalidOutput => "platform.mod.invalid_output",
            Self::InvalidWorldState => "platform.mod.invalid_world_state",
            Self::FuelExceeded => "platform.mod.resource.fuel_exceeded",
            Self::MemoryOutOfBounds => "platform.mod.resource.memory_out_of_bounds",
            Self::StackExceeded => "platform.mod.resource.stack_exceeded",
            Self::QuotaExceeded => "platform.mod.resource.quota_exceeded",
            Self::GuestTrapUnreachable => "platform.mod.guest_trap.unreachable",
            Self::GuestTrapIntegerOverflow => "platform.mod.guest_trap.integer_overflow",
            Self::GuestTrapIntegerDivideByZero => "platform.mod.guest_trap.integer_divide_by_zero",
            Self::GuestTrapInvalidConversion => "platform.mod.guest_trap.invalid_conversion",
            Self::GuestTrapInterrupted => "platform.mod.guest_trap.interrupted",
            Self::GuestTrapOther => "platform.mod.guest_trap.other",
            Self::GuestTrapUnknown => "platform.mod.guest_trap.unknown",
            Self::ApiInvocationFailed => "platform.mod.api.invocation_failed",
            Self::MemoryLimitExceeded => "platform.mod.resource.memory_limit_exceeded",
            Self::StackLimitExceeded => "platform.mod.resource.stack_limit_exceeded",
            Self::LoadFailed => "platform.mod.load_failed",
            Self::InitializationFailed => "platform.mod.initialization_failed",
        }
    }

    /// Maps a stable code produced by [`classify_instantiation_error`] back to a
    /// [`ModFailureCode`], falling back to `LoadFailed` for anything else.
    fn from_instantiation_code(code: &str) -> Self {
        match code {
            "platform.mod.resource.memory_limit_exceeded" => Self::MemoryLimitExceeded,
            "platform.mod.resource.stack_limit_exceeded" => Self::StackLimitExceeded,
            "platform.mod.resource.fuel_exceeded" => Self::FuelExceeded,
            "platform.mod.resource.memory_out_of_bounds" => Self::MemoryOutOfBounds,
            "platform.mod.resource.stack_exceeded" => Self::StackExceeded,
            _ => Self::LoadFailed,
        }
    }

    /// Maps a runtime [`Trap`] to the frozen stable code (see [`classify_runtime_trap`]).
    pub const fn from_trap(trap: &Trap) -> Self {
        match trap {
            Trap::OutOfFuel => Self::FuelExceeded,
            Trap::MemoryOutOfBounds => Self::MemoryOutOfBounds,
            Trap::StackOverflow => Self::StackExceeded,
            Trap::UnreachableCodeReached => Self::GuestTrapUnreachable,
            Trap::IntegerOverflow => Self::GuestTrapIntegerOverflow,
            Trap::IntegerDivisionByZero => Self::GuestTrapIntegerDivideByZero,
            Trap::BadConversionToInteger => Self::GuestTrapInvalidConversion,
            Trap::Interrupt => Self::GuestTrapInterrupted,
            Trap::TableOutOfBounds
            | Trap::IndirectCallToNull
            | Trap::BadSignature
            | Trap::NullReference => Self::GuestTrapOther,
            _ => Self::GuestTrapUnknown,
        }
    }
}

// ---------------------------------------------------------------------------
// GK-P3-06: unified, versioned fault report contract.
//
// Design source: planning/16-P3-06-故障报告契约设计提案.md (rev2, FROZEN) and
// planning/drafts/mod-fault-report-v1.schema.DRAFT.json. The report is a
// Platform-owned type (`ModFaultReport`); the Kernel does NOT depend on it and
// performs no reverse conversion. `message` is human-readable only and is NOT a
// machine protocol field: every machine decision (category, phase, severity,
// disposition, retryability, authority) is derived from the stable `code`,
// never from `message` text.
// ---------------------------------------------------------------------------

/// Version of the [`ModFaultReport`] document format.
pub const FAULT_REPORT_VERSION: u32 = 1;

/// The fourteen error categories required by `Mod Platform Specification` §12.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultCategory {
    Manifest,
    Package,
    Dependency,
    VersionConflict,
    Permission,
    Load,
    Initialization,
    ApiRegistration,
    ApiInvocation,
    WasmTrap,
    ResourceLimit,
    /// Category reserved for §12.1 `DeterminismViolation`.
    ///
    /// NOTE: only the category and its stable code exist in this build. The
    /// actual violation-detection capability is NOT implemented (see the
    /// O-6-1 unmet item in the frozen design). No detection result may be
    /// fabricated for this category.
    Determinism,
    Lifecycle,
    /// Category reserved for §12.1 `MigrationError`.
    ///
    /// NOTE: only the category and its stable code exist in this build. The
    /// actual migration-failure detection capability is NOT implemented (see
    /// the O-6-2 unmet item in the frozen design). No detection result may be
    /// fabricated for this category.
    Migration,
}

impl FaultCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Package => "package",
            Self::Dependency => "dependency",
            Self::VersionConflict => "version_conflict",
            Self::Permission => "permission",
            Self::Load => "load",
            Self::Initialization => "initialization",
            Self::ApiRegistration => "api_registration",
            Self::ApiInvocation => "api_invocation",
            Self::WasmTrap => "wasm_trap",
            Self::ResourceLimit => "resource_limit",
            Self::Determinism => "determinism",
            Self::Lifecycle => "lifecycle",
            Self::Migration => "migration",
        }
    }
}

/// Lifecycle stage in which a fault occurred (§12.4 "错误发生阶段").
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultPhase {
    Discovery,
    Manifest,
    Package,
    Resolution,
    Lockfile,
    Permission,
    Compile,
    Instantiation,
    Registration,
    Initialization,
    Execution,
    Teardown,
}

impl FaultPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Manifest => "manifest",
            Self::Package => "package",
            Self::Resolution => "resolution",
            Self::Lockfile => "lockfile",
            Self::Permission => "permission",
            Self::Compile => "compile",
            Self::Instantiation => "instantiation",
            Self::Registration => "registration",
            Self::Initialization => "initialization",
            Self::Execution => "execution",
            Self::Teardown => "teardown",
        }
    }
}

/// Fault-handling scope (§12.2 / Security spec §6.1: L1 call .. L4 runtime).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityGrade {
    L1Call,
    L2Task,
    L3Mod,
    L4Runtime,
}

impl SeverityGrade {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::L1Call => "l1_call",
            Self::L2Task => "l2_task",
            Self::L3Mod => "l3_mod",
            Self::L4Runtime => "l4_runtime",
        }
    }
}

/// What the platform should do next for a fault (task card: continue/stop/rebuild).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Continue,
    Stop,
    Rebuild,
}

impl Disposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Stop => "stop",
            Self::Rebuild => "rebuild",
        }
    }
}

/// Structured recovery advice (enum, never free text).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAdvice {
    None,
    RetryCall,
    DisableMod,
    RebuildInstance,
    RestoreCheckpoint,
    FixManifest,
    FixPackage,
}

impl RecoveryAdvice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RetryCall => "retry_call",
            Self::DisableMod => "disable_mod",
            Self::RebuildInstance => "rebuild_instance",
            Self::RestoreCheckpoint => "restore_checkpoint",
            Self::FixManifest => "fix_manifest",
            Self::FixPackage => "fix_package",
        }
    }
}

/// Resource budget kinds of `Mod Platform Specification` §10.1.
///
/// The current default values are configurable and are NOT a frozen contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    FuelPerCall,
    PerTickFuel,
    Memory,
    CommandsPerTick,
    EventsPerTick,
    Tasks,
    ApiCalls,
    PersistentBytes,
    LogBytes,
}

impl LimitKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FuelPerCall => "fuel_per_call",
            Self::PerTickFuel => "per_tick_fuel",
            Self::Memory => "memory",
            Self::CommandsPerTick => "commands_per_tick",
            Self::EventsPerTick => "events_per_tick",
            Self::Tasks => "tasks",
            Self::ApiCalls => "api_calls",
            Self::PersistentBytes => "persistent_bytes",
            Self::LogBytes => "log_bytes",
        }
    }
}

/// Resource usage attached to a fault report when a budget was involved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUse {
    pub limit_kind: LimitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_value: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_value: Option<u64>,
    /// Version of the budget configuration the values came from (Security spec §5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u32>,
    pub exceeded: bool,
}

/// Dependency context attached to a fault report when dependencies are involved.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycle: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_consumers: Vec<String>,
}

/// Rollback / recovery outcome (§12.4 "是否已执行回滚或恢复").
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackOutcome {
    pub executed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_work: Option<bool>,
}

/// Versioned, machine-readable Platform fault report.
///
/// Independent of the Kernel's `SystemError`. `message` is human-readable only
/// and is not a machine protocol field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModFaultReport {
    pub report_version: u32,
    pub code: String,
    pub category: FaultCategory,
    pub phase: FaultPhase,
    pub severity_grade: SeverityGrade,
    pub disposition: Disposition,
    pub authoritative: bool,
    pub retryable: bool,
    pub recovery_advice: RecoveryAdvice,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_context: Option<DependencyContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackOutcome>,

    pub message: String,
}

impl ModFaultReport {
    /// Builds a report from a stable code and the derived machine decision.
    ///
    /// The decision (category/phase/severity/disposition/retryable/advice and
    /// authority) is looked up from the frozen mapping table by `code`; it is
    /// never derived from `message`.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let decision = fault_decision(&code);
        Self {
            report_version: FAULT_REPORT_VERSION,
            code,
            category: decision.category,
            phase: decision.phase,
            severity_grade: decision.severity_grade,
            disposition: decision.disposition,
            authoritative: decision.authoritative,
            retryable: decision.retryable,
            recovery_advice: decision.recovery_advice,
            mod_id: None,
            mod_version: None,
            manifest_version: None,
            kernel_api_version: None,
            platform_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            load_plan_id: None,
            instance_id: None,
            content_digest: None,
            tick: None,
            scheduler_context: None,
            api_id: None,
            system_id: None,
            resource: None,
            dependency_context: None,
            rollback: None,
            message: message.into(),
        }
    }

    pub fn with_mod_id(mut self, mod_id: impl Into<String>) -> Self {
        self.mod_id = Some(mod_id.into());
        self
    }

    pub fn with_content_digest(mut self, digest: impl Into<String>) -> Self {
        self.content_digest = Some(digest.into());
        self
    }

    pub fn with_tick(mut self, tick: u64) -> Self {
        self.tick = Some(tick);
        self
    }

    pub fn with_resource(mut self, resource: ResourceUse) -> Self {
        self.resource = Some(resource);
        self
    }

    pub fn with_dependency_context(mut self, context: DependencyContext) -> Self {
        self.dependency_context = Some(context);
        self
    }

    pub fn with_rollback(mut self, rollback: RollbackOutcome) -> Self {
        self.rollback = Some(rollback);
        self
    }

    /// Fills Mod identity fields from a manifest.
    pub fn with_manifest(mut self, manifest: &ModManifest) -> Self {
        self.mod_id = Some(manifest.id.clone());
        self.mod_version = Some(manifest.version.clone());
        self.manifest_version = Some(manifest.manifest_version);
        self.kernel_api_version = Some(manifest.kernel_api.clone());
        self
    }

    /// Serializes the report to its canonical, Schema-validatable JSON form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("fault report serializes");
        bytes.push(b'\n');
        bytes
    }

    /// Maps a Kernel `SystemError` to a Platform report.
    ///
    /// This is a one-way read: the Kernel never depends on [`ModFaultReport`],
    /// and no reverse conversion is provided. Per the frozen §6.2 mapping:
    /// `code` -> `code`; `system` -> `system_id`; `tick` -> `tick`;
    /// `source_digest` -> `content_digest`; `message` is copied verbatim into
    /// the report's non-protocol `message` field.
    pub fn from_system_error(error: &SystemError) -> Self {
        let mut report =
            Self::new(error.code.clone(), error.message.clone()).with_tick(error.tick.0);
        report.system_id = Some(error.system.clone());
        if let Some(digest) = &error.source_digest {
            report.content_digest = Some(digest.clone());
        }
        report
    }

    /// Maps a Platform error to a report using the frozen category mapping.
    pub fn from_platform_error(error: &PlatformError) -> Self {
        let code = error.fault_code();
        let mut report = Self::new(code, error.to_string());
        match error {
            PlatformError::PermissionDenied { mod_id, .. }
            | PlatformError::PermissionNotRequested { mod_id, .. }
            | PlatformError::WasmModule { mod_id, .. }
            | PlatformError::ApiSignatureMismatch { mod_id, .. }
            | PlatformError::ModLoad { mod_id, .. } => report.mod_id = Some(mod_id.clone()),
            PlatformError::Lifecycle(failure) => {
                report.mod_id = failure.mod_id.clone();
            }
            PlatformError::DependencyResolution(failure) => {
                report.mod_id = failure.mod_id.clone();
                report.dependency_context = Some(DependencyContext {
                    dependency_id: failure.dependency_id.clone(),
                    required: Some(true),
                    cycle: failure.cycle.clone(),
                    affected_consumers: Vec::new(),
                });
            }
            PlatformError::LockFile(failure) => {
                report.mod_id = failure.mod_id.clone();
            }
            _ => {}
        }
        report
    }
}

/// The machine decision derived from a stable code (see frozen §3/§4/§5 tables).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FaultDecision {
    category: FaultCategory,
    phase: FaultPhase,
    severity_grade: SeverityGrade,
    disposition: Disposition,
    authoritative: bool,
    retryable: bool,
    recovery_advice: RecoveryAdvice,
}

/// Looks up the frozen decision for a stable code.
///
/// A code outside the frozen table is treated conservatively as a runtime-level
/// fault that must rebuild and cannot be safely continued (§5 row 8: never
/// default to `continue`).
fn fault_decision(code: &str) -> FaultDecision {
    let (category, phase, severity_grade, disposition, authoritative, retryable, recovery_advice) =
        match code {
            "platform.invalid_manifest" => (
                FaultCategory::Manifest,
                FaultPhase::Manifest,
                SeverityGrade::L1Call,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::FixManifest,
            ),
            "platform.invalid_package" => (
                FaultCategory::Package,
                FaultPhase::Package,
                SeverityGrade::L1Call,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::FixPackage,
            ),
            "platform.dependency_resolution"
            | "platform.dependency.duplicate_mod_id"
            | "platform.dependency.duplicate_dependency"
            | "platform.dependency.missing_dependency"
            | "platform.dependency.invalid_version_requirement"
            | "platform.dependency.cycle" => (
                FaultCategory::Dependency,
                FaultPhase::Resolution,
                SeverityGrade::L1Call,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::FixManifest,
            ),
            "platform.dependency.version_incompatible" => (
                FaultCategory::VersionConflict,
                FaultPhase::Resolution,
                SeverityGrade::L1Call,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::FixManifest,
            ),
            "platform.permission_denied" | "platform.permission_not_requested" => (
                FaultCategory::Permission,
                FaultPhase::Permission,
                SeverityGrade::L1Call,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::FixManifest,
            ),
            "platform.mod.load_failed" => (
                FaultCategory::Load,
                FaultPhase::Instantiation,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.mod.initialization_failed" => (
                FaultCategory::Initialization,
                FaultPhase::Initialization,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.api_catalog" | "platform.api_signature_mismatch" | "platform.wasm_module" => {
                (
                    FaultCategory::ApiRegistration,
                    FaultPhase::Registration,
                    SeverityGrade::L2Task,
                    Disposition::Stop,
                    false,
                    false,
                    RecoveryAdvice::DisableMod,
                )
            }
            "platform.mod.api.invocation_failed" => (
                FaultCategory::ApiInvocation,
                FaultPhase::Execution,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.mod.guest_trap.interrupted" => (
                FaultCategory::WasmTrap,
                FaultPhase::Execution,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                true,
                RecoveryAdvice::RetryCall,
            ),
            c if c.starts_with("platform.mod.guest_trap.")
                || c == "platform.mod.invalid_world_state"
                || c == "platform.mod.invalid_output"
                || c == "platform.mod.execution_failed" =>
            {
                (
                    FaultCategory::WasmTrap,
                    FaultPhase::Execution,
                    SeverityGrade::L2Task,
                    Disposition::Stop,
                    false,
                    false,
                    RecoveryAdvice::DisableMod,
                )
            }
            "platform.mod.resource.fuel_exceeded" => (
                FaultCategory::ResourceLimit,
                FaultPhase::Execution,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                true,
                RecoveryAdvice::RetryCall,
            ),
            "platform.mod.resource.memory_limit_exceeded"
            | "platform.mod.resource.stack_limit_exceeded" => (
                FaultCategory::ResourceLimit,
                FaultPhase::Instantiation,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.mod.resource.memory_out_of_bounds"
            | "platform.mod.resource.stack_exceeded"
            | "platform.mod.resource.quota_exceeded" => (
                FaultCategory::ResourceLimit,
                FaultPhase::Execution,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.mod.determinism_violation" => (
                FaultCategory::Determinism,
                FaultPhase::Execution,
                SeverityGrade::L4Runtime,
                Disposition::Rebuild,
                true,
                false,
                RecoveryAdvice::RebuildInstance,
            ),
            "platform.mod.migration_failed" => (
                FaultCategory::Migration,
                FaultPhase::Teardown,
                SeverityGrade::L4Runtime,
                Disposition::Rebuild,
                true,
                false,
                RecoveryAdvice::RestoreCheckpoint,
            ),
            c if c.starts_with("platform.lifecycle") => (
                FaultCategory::Lifecycle,
                FaultPhase::Registration,
                SeverityGrade::L3Mod,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            "platform.mod.runtime_setup" => (
                FaultCategory::Load,
                FaultPhase::Instantiation,
                SeverityGrade::L2Task,
                Disposition::Stop,
                false,
                false,
                RecoveryAdvice::DisableMod,
            ),
            // Conservative default (§5.3 row 8): never default to continue.
            _ => (
                FaultCategory::WasmTrap,
                FaultPhase::Execution,
                SeverityGrade::L4Runtime,
                Disposition::Rebuild,
                true,
                false,
                RecoveryAdvice::RebuildInstance,
            ),
        };
    FaultDecision {
        category,
        phase,
        severity_grade,
        disposition,
        authoritative,
        retryable,
        recovery_advice,
    }
}

/// Classifies a Wasmtime error raised at instantiation time.
///
/// The instantiation-phase linear-memory limit is surfaced by Wasmtime as a
/// plain error (not a `Trap` and not `OutOfMemory`), so it is matched on its
/// stable mechanism rather than its localized message text where possible.
fn classify_instantiation_error(error: &wasmtime::Error) -> (&'static str, &'static str) {
    if error.downcast_ref::<wasmtime::OutOfMemory>().is_some() {
        return (
            "platform.mod.resource.memory_limit_exceeded",
            "instantiation exceeded a configured Wasm resource limit",
        );
    }
    if let Some(trap) = error.downcast_ref::<Trap>() {
        let code = classify_runtime_trap(trap);
        return (code, "instantiation trapped");
    }
    let text = error.to_string();
    if text.contains("exceeds memory limits") {
        return (
            "platform.mod.resource.memory_limit_exceeded",
            "module linear memory exceeds the configured memory limit",
        );
    }
    if text.contains("exceeds the limit") || text.contains("too many") {
        return (
            "platform.mod.resource.memory_limit_exceeded",
            "module exceeds a configured instantiation limit",
        );
    }
    ("platform.mod.load_failed", "module failed to instantiate")
}

/// Maps a runtime Wasmtime [`Trap`] to a stable code (frozen §3.2/§3.3 tables).
///
/// The classification is based on the typed `Trap` variant only; never on the
/// trap name string or on message text. This is a thin alias over
/// [`ModFailureCode::from_trap`] so there is a single source of truth.
pub(crate) fn classify_runtime_trap(trap: &Trap) -> &'static str {
    ModFailureCode::from_trap(trap).as_str()
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
    /// GK-P3-06: a load/instantiation-phase failure carrying a frozen stable
    /// code (e.g. `platform.mod.resource.memory_limit_exceeded` for an
    /// instantiation-phase linear-memory limit, or `platform.mod.load_failed`).
    ///
    /// `code` is the machine protocol field; `message` is human-readable only.
    #[error("Mod `{mod_id}` failed to load ({code}): {message}")]
    ModLoad {
        mod_id: String,
        code: &'static str,
        message: String,
    },
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
    #[error("Mod lifecycle or registration failure: {0}")]
    Lifecycle(Box<LifecycleFailure>),
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
    /// GK-P3-06: load/instantiation failure with a precise frozen code.
    ModLoad,
    ApiSignatureMismatch,
    Serialization,
    InvalidSchema,
    LockFile,
    ApiCatalog,
    Lifecycle,
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
            Self::ModLoad => "platform.mod.load",
            Self::ApiSignatureMismatch => "platform.api_signature_mismatch",
            Self::Serialization => "platform.serialization",
            Self::InvalidSchema => "platform.invalid_schema",
            Self::LockFile => "platform.lock_file",
            Self::ApiCatalog => "platform.api_catalog",
            Self::Lifecycle => "platform.lifecycle",
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

    /// Builds a lifecycle/registration failure with the same boxing rationale
    /// as [`PlatformError::dependency`].
    fn lifecycle(failure: LifecycleFailure) -> Self {
        Self::Lifecycle(Box::new(failure))
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
            Self::ModLoad { .. } => PlatformErrorCode::ModLoad,
            Self::ApiSignatureMismatch { .. } => PlatformErrorCode::ApiSignatureMismatch,
            Self::Serialization(_) => PlatformErrorCode::Serialization,
            Self::InvalidSchema(_) => PlatformErrorCode::InvalidSchema,
            Self::LockFile(_) => PlatformErrorCode::LockFile,
            Self::ApiCatalog(_) => PlatformErrorCode::ApiCatalog,
            Self::Lifecycle(_) => PlatformErrorCode::Lifecycle,
        }
    }

    /// The precise frozen fault code for this error, when it carries one.
    ///
    /// For [`PlatformError::ModLoad`] this is the fine-grained stable code
    /// (`platform.mod.resource.memory_limit_exceeded`, `platform.mod.load_failed`,
    /// ...). For every other variant it is the same value as [`Self::code`].
    pub const fn fault_code(&self) -> &'static str {
        match self {
            Self::ModLoad { code, .. } => code,
            _ => self.code().as_str(),
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
            .map_err(|error| self.instantiation_error(tick, &error))?;
        let on_tick = instance
            .get_typed_func::<(), ()>(&mut store, "on_tick")
            .map_err(|error| self.instantiation_error(tick, &error))?;
        on_tick
            .call(&mut store, ())
            .map_err(|error| self.execution_error(tick, &error))?;

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

    /// Builds the `SystemError` for a failure raised while the Mod's `on_tick`
    /// is running.
    ///
    /// A wasmtime trap is classified by its typed variant into the frozen
    /// §3.2/§3.3 stable code; an error that is not a trap keeps the
    /// conservative `platform.mod.execution_failed` fallback. The classification
    /// never consults the message text.
    fn execution_error(&self, tick: Tick, error: &wasmtime::Error) -> SystemError {
        let code = match error.downcast_ref::<Trap>() {
            Some(trap) => ModFailureCode::from_trap(trap),
            None => ModFailureCode::ExecutionFailed,
        };
        self.system_error(tick, code, error.to_string())
    }

    /// Builds the `SystemError` for a failure raised while instantiating the
    /// Mod's store/instance at execution time.
    ///
    /// Instantiation-phase limits (frozen §3.3 C.2) are classified separately
    /// from runtime traps; anything else is `platform.mod.load_failed`.
    fn instantiation_error(&self, tick: Tick, error: &wasmtime::Error) -> SystemError {
        let code = ModFailureCode::from_instantiation_code(classify_instantiation_error(error).0);
        self.system_error(tick, code, error.to_string())
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
    // GK-P3-06: an instantiation-phase failure is classified by mechanism into
    // its frozen stable code (§3.3 C.2), rather than being folded into the
    // generic `platform.wasm_module` code.
    let instance = linker.instantiate(&mut store, module).map_err(|error| {
        let (code, _) = classify_instantiation_error(&error);
        PlatformError::ModLoad {
            mod_id: mod_id.to_owned(),
            code,
            message: error.to_string(),
        }
    })?;
    instance
        .get_typed_func::<(), ()>(&mut store, "on_tick")
        .map_err(|error| PlatformError::ModLoad {
            mod_id: mod_id.to_owned(),
            code: "platform.mod.load_failed",
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

/// The embedded `mod-fault-report-v1` schema, exposed so contract tests can
/// prove the report's exported bytes satisfy the published schema.
pub const MOD_FAULT_REPORT_SCHEMA: &str =
    include_str!("../../../schemas/mod-fault-report-v1.schema.json");

/// Validates a serialized fault-report value against the embedded v1 schema.
///
/// The Schema is the frozen structural half of the GK-P3-06 contract; the
/// category/phase/disposition decision values are additionally fixed by the
/// report's stable `code` (see [`ModFaultReport::new`]).
pub fn validate_fault_report_value(value: &serde_json::Value) -> Result<(), String> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value =
            serde_json::from_str(MOD_FAULT_REPORT_SCHEMA).map_err(|error| error.to_string())?;
        jsonschema::validator_for(&schema).map_err(|error| error.to_string())
    });
    let validator = validator.as_ref().map_err(|error| error.clone())?;
    if let Err(error) = validator.validate(value) {
        return Err(format!(
            "fault report schema violation at `{}`: {error}",
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

// ---------------------------------------------------------------------------
// Mod lifecycle state machine (§7.1 of `Mod Platform Specification v1.0`)
// ---------------------------------------------------------------------------

/// Observable lifecycle state of a single Mod, as enumerated by
/// `Mod Platform Specification v1.0` §7.1.
///
/// The ten states are exactly the spec set. `Disabled` is deliberately **not**
/// a variant: the spec describes it as wanting new work but not equal to
/// `Stopped`, and §7.1 gives no transitions for it. Modelling it here would be
/// inventing semantics the spec does not define, so it is left as a
/// to-be-defined item rather than folded into `Stopped`.
///
/// # Hot reload / unload is not supported
///
/// This build does **not** implement runtime hot reload or re-load of a Mod.
/// `Unloaded` and `Failed` are conservative terminal states in this
/// implementation (see [`ModLifecycleState::can_transition_to`]), so there is no
/// modeled path back from them into the lifecycle; a Mod cannot be re-activated
/// in place. Recovery is expected to rebuild from a trusted snapshot rather than
/// to reload a running instance. This statement adds **no** new transition
/// rules and does not define `Disabled`, `Failed` or `Unloaded` semantics beyond
/// what §7.1 and the existing transition table already say.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModLifecycleState {
    /// Package discovered and its origin recorded.
    Discovered,
    /// Manifest, files and compatibility resolved and validated.
    Validated,
    /// Dependencies and load order confirmed.
    Resolved,
    /// Execution instance created, module code loaded.
    Loaded,
    /// Interfaces registered, initial runtime resources created.
    Initialized,
    /// Normal scheduling, events and API calls accepted.
    Active,
    /// New work refused, permitted cleanup in progress.
    Stopping,
    /// No longer accepting runtime work.
    Stopped,
    /// Execution instance and managed resources released.
    Unloaded,
    /// Error state reachable from multiple stages; records stage and reason.
    Failed,
}

impl ModLifecycleState {
    /// Stable machine-readable name, matching the spec's spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "Discovered",
            Self::Validated => "Validated",
            Self::Resolved => "Resolved",
            Self::Loaded => "Loaded",
            Self::Initialized => "Initialized",
            Self::Active => "Active",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
            Self::Unloaded => "Unloaded",
            Self::Failed => "Failed",
        }
    }

    /// Whether a transition into `next` is permitted by this implementation.
    ///
    /// The forward chain is `Discovered -> Validated -> Resolved -> Loaded ->
    /// Initialized -> Active -> Stopping -> Stopped`, with `Unloaded` reachable
    /// only from `Stopped`, and `Failed` reachable from any non-terminal stage
    /// (`Failed` itself never has to terminate the chain, matching "reachable
    /// from multiple stages").
    ///
    /// **Current-implementation strategy, not spec-mandated terminality:**
    /// §7.1 does not state whether `Unloaded` or `Failed` have outgoing
    /// transitions. This build takes the conservative position that both are
    /// terminal and rejects any outgoing transition. This is recorded as a spec
    /// gap; it must not be read as the spec declaring them terminal.
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Discovered => matches!(next, Self::Validated | Self::Failed),
            Self::Validated => matches!(next, Self::Resolved | Self::Failed),
            Self::Resolved => matches!(next, Self::Loaded | Self::Failed),
            Self::Loaded => matches!(next, Self::Initialized | Self::Failed),
            Self::Initialized => matches!(next, Self::Active | Self::Failed),
            Self::Active => matches!(next, Self::Stopping | Self::Failed),
            Self::Stopping => matches!(next, Self::Stopped | Self::Failed),
            Self::Stopped => matches!(next, Self::Unloaded | Self::Failed),
            // Conservative terminal handling; see the doc comment above.
            Self::Unloaded | Self::Failed => false,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Unloaded | Self::Failed)
    }
}

/// Stable classification for lifecycle and registration failures. Diagnostics
/// are human-readable and may change; machines must match on these codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LifecycleCode {
    /// A transition not permitted from the current state was requested.
    #[serde(rename = "platform.lifecycle.illegal_transition")]
    IllegalTransition,
    /// Two Systems in one batch declared the same Mod ID.
    #[serde(rename = "platform.lifecycle.duplicate_mod_id")]
    DuplicateModId,
    /// A Mod ID already recorded as registered was submitted again.
    #[serde(rename = "platform.lifecycle.already_registered")]
    AlreadyRegistered,
    /// A Mod ID failed Kernel's own `validate_type_id` rule.
    #[serde(rename = "platform.lifecycle.invalid_mod_id")]
    InvalidModId,
    /// The caller-supplied Mod ID did not equal `System::id()`.
    #[serde(rename = "platform.lifecycle.mod_id_mismatch")]
    ModIdMismatch,
    /// Kernel rejected the registration during the real registration call.
    #[serde(rename = "platform.lifecycle.registration_rejected")]
    RegistrationRejected,
}

impl LifecycleCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IllegalTransition => "platform.lifecycle.illegal_transition",
            Self::DuplicateModId => "platform.lifecycle.duplicate_mod_id",
            Self::AlreadyRegistered => "platform.lifecycle.already_registered",
            Self::InvalidModId => "platform.lifecycle.invalid_mod_id",
            Self::ModIdMismatch => "platform.lifecycle.mod_id_mismatch",
            Self::RegistrationRejected => "platform.lifecycle.registration_rejected",
        }
    }
}

/// Structured lifecycle / registration failure. Carries a stable code, the
/// phase at which a batch failed, and the Mod ID the failure refers to;
/// `message` is diagnostic text and is not part of the machine protocol.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleFailure {
    pub code: LifecycleCode,
    /// Mod whose transition or registration failed, when one is identifiable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    /// State the Mod was in when the transition was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ModLifecycleState>,
    /// State the transition attempted to reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ModLifecycleState>,
    /// Zero-based index of the batch element that failed, when a batch failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<usize>,
    /// Human-readable diagnostic. Not part of the machine protocol.
    pub message: String,
}

impl LifecycleFailure {
    fn new(code: LifecycleCode, message: impl Into<String>) -> Self {
        Self {
            code,
            mod_id: None,
            from: None,
            to: None,
            batch_index: None,
            message: message.into(),
        }
    }

    fn with_mod(mut self, mod_id: impl Into<String>) -> Self {
        self.mod_id = Some(mod_id.into());
        self
    }

    fn with_transition(mut self, from: ModLifecycleState, to: ModLifecycleState) -> Self {
        self.from = Some(from);
        self.to = Some(to);
        self
    }

    fn with_batch_index(mut self, index: usize) -> Self {
        self.batch_index = Some(index);
        self
    }

    /// Stable machine-readable code.
    pub const fn code(&self) -> LifecycleCode {
        self.code
    }
}

impl std::fmt::Display for LifecycleFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Per-Mod lifecycle tracker. Owns one Mod's current state and validates every
/// transition against [`ModLifecycleState::can_transition_to`].
///
/// This type deliberately owns **only** lifecycle state. The set of
/// successfully registered System IDs is owned by [`ModRegistry`]; the two
/// responsibilities are kept apart so lifecycle bookkeeping can never silently
/// stand in for registration truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModLifecycle {
    mod_id: String,
    state: ModLifecycleState,
    /// History of accepted transitions, oldest first, for diagnostics.
    history: Vec<ModLifecycleState>,
}

impl ModLifecycle {
    /// Starts a Mod at `Discovered`, the entry state of §7.1.
    pub fn discovered(mod_id: impl Into<String>) -> Self {
        let mod_id = mod_id.into();
        Self {
            history: vec![ModLifecycleState::Discovered],
            mod_id,
            state: ModLifecycleState::Discovered,
        }
    }

    pub fn mod_id(&self) -> &str {
        &self.mod_id
    }

    pub fn state(&self) -> ModLifecycleState {
        self.state
    }

    pub fn history(&self) -> &[ModLifecycleState] {
        &self.history
    }

    /// Attempts to advance to `next`. Illegal transitions are rejected with a
    /// diagnosable [`LifecycleFailure`] and leave the current state unchanged.
    pub fn transition(&mut self, next: ModLifecycleState) -> Result<(), PlatformError> {
        if !self.state.can_transition_to(next) {
            return Err(PlatformError::lifecycle(
                LifecycleFailure::new(
                    LifecycleCode::IllegalTransition,
                    format!(
                        "illegal lifecycle transition for Mod `{}`: {} -> {}",
                        self.mod_id,
                        self.state.as_str(),
                        next.as_str()
                    ),
                )
                .with_mod(self.mod_id.clone())
                .with_transition(self.state, next),
            ));
        }
        self.state = next;
        self.history.push(next);
        Ok(())
    }

    /// Marks the Mod failed, recording the failure stage. Legal from every
    /// non-terminal stage; a Mod already terminal is rejected.
    pub fn fail(&mut self) -> Result<(), PlatformError> {
        self.transition(ModLifecycleState::Failed)
    }
}

/// Registration coordinator. Owns the set of Mod IDs whose Systems have been
/// **successfully** registered with a `Kernel`, and routes batch registration
/// through a single pre-validated entry point.
///
/// # Consistency boundary
///
/// This coordinator constrains registrations that flow through it. It can
/// **neither discover nor prevent** a caller that invokes
/// `Kernel::register_system` directly: the Kernel exposes that method to
/// everyone, and a direct call will not update this record. Any statement about
/// "global registration consistency" is therefore false by construction.
///
/// # What the batch guarantee does and does not cover
///
/// For a batch submitted through [`ModRegistry::register_batch`], the effective
/// registration ID of every element is **frozen during pre-validation** and the
/// Kernel is handed a wrapper whose `id()` always returns that frozen value
/// (see [`FrozenIdSystem`]). Consequently:
///
/// - An unstable `System::id()` can no longer make the Kernel reject a later
///   element after eager elements were already committed. The `id()`-drift
///   class of partial commit is closed.
/// - For a Kernel that holds **only** coordinator-registered Systems, every
///   pre-validation failure mode is decided before any Kernel mutation, so the
///   batch is all-or-nothing.
/// - It is **not** an unconditional atomicity claim. If the target Kernel
///   already contains a System registered *outside* this coordinator, the
///   Kernel may still reject an element mid-batch, leaving earlier elements
///   committed. That residual case is the pre-existing bypass limitation, not
///   an `id()`-stability issue, and is exercised by
///   `bypass_boundary_direct_kernel_registration_does_not_update_the_record`.
///
/// This is a constructive guarantee for coordinator-submitted batches. It is
/// **not** a Kernel-provided transaction or rollback facility.
#[derive(Clone, Debug, Default)]
pub struct ModRegistry {
    registered: BTreeSet<String>,
}

/// A `System` wrapper that pins the identity the Kernel observes.
///
/// The Kernel reads a System's identity by calling `System::id()` — once to
/// validate, again to scan for duplicates, and again when sorting. A `System`
/// whose `id()` returns a different value on different calls can therefore pass
/// pre-validation and still fail (or be recorded under the wrong name) during
/// registration. Wrapping the System in this adapter makes `id()` return one
/// frozen string for the entire registration, so the identity that was
/// pre-validated is exactly the identity the Kernel commits.
///
/// `execute` is delegated to the wrapped System unchanged.
struct FrozenIdSystem {
    id: String,
    inner: Box<dyn System>,
}

impl FrozenIdSystem {
    fn new(id: String, inner: Box<dyn System>) -> Self {
        Self { id, inner }
    }
}

impl System for FrozenIdSystem {
    fn id(&self) -> &str {
        &self.id
    }

    fn execute(&self, world: &World, tick: Tick) -> Result<Vec<Command>, SystemError> {
        self.inner.execute(world, tick)
    }
}

impl ModRegistry {
    pub fn new() -> Self {
        Self {
            registered: BTreeSet::new(),
        }
    }

    /// Mod IDs recorded as successfully registered, in stable order.
    pub fn registered_ids(&self) -> impl Iterator<Item = &str> {
        self.registered.iter().map(String::as_str)
    }

    /// Number of Mod IDs recorded as successfully registered.
    pub fn registered_count(&self) -> usize {
        self.registered.len()
    }

    pub fn contains(&self, mod_id: &str) -> bool {
        self.registered.contains(mod_id)
    }

    /// Registers a batch of Systems under one pre-validation pass.
    ///
    /// The input is consuming and ownership-transferring: each element pairs the
    /// caller-declared Mod ID with the owned `System`, so the coordinator can
    /// take the System and hand it to the Kernel without a second borrow.
    ///
    /// Each element's identity is read from `System::id()` **exactly once**
    /// during pre-validation and then frozen. Phase 2 hands the Kernel a
    /// [`FrozenIdSystem`] carrying that frozen identity, so the ID the Kernel
    /// validates, de-duplicates and sorts by is always the ID that was
    /// pre-validated — even if the underlying `System::id()` is not stable
    /// across calls. This is what removes the `id()`-drift class of partial
    /// commit.
    ///
    /// Pre-validation runs **before any Kernel mutation** and covers, in order:
    /// 1. the declared Mod ID equalling the System's (single) `id()` read;
    /// 2. within-batch duplicate Mod IDs;
    /// 3. Mod IDs already recorded as registered;
    /// 4. Kernel's own ID rule, reusing `kernel_core::validate_type_id` so this
    ///    layer never maintains a second, drifting rule set.
    ///
    /// Only if every element passes is the batch committed to the Kernel. Each
    /// Kernel registration is still fallible (see the type-level note on the
    /// residual bypass case), and a rejection fails the whole batch with
    /// [`LifecycleCode::RegistrationRejected`]. IDs are written to the record
    /// **only after** the corresponding `register_system` call returns `Ok`.
    pub fn register_batch(
        &mut self,
        kernel: &mut Kernel,
        systems: Vec<(String, Box<dyn System>)>,
    ) -> Result<(), PlatformError> {
        // Phase 1: pre-validation. No Kernel state is touched here. Each id()
        // is read exactly once and carried forward as the frozen identity.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut pending: Vec<(String, Box<dyn System>)> = Vec::with_capacity(systems.len());
        for (index, (declared_id, system)) in systems.into_iter().enumerate() {
            let actual_id = system.id().to_owned();
            if declared_id != actual_id {
                return Err(PlatformError::lifecycle(
                    LifecycleFailure::new(
                        LifecycleCode::ModIdMismatch,
                        format!(
                            "declared Mod ID `{declared_id}` does not match System::id() `{actual_id}`"
                        ),
                    )
                    .with_mod(declared_id.clone())
                    .with_batch_index(index),
                ));
            }
            if !seen.insert(actual_id.clone()) {
                return Err(PlatformError::lifecycle(
                    LifecycleFailure::new(
                        LifecycleCode::DuplicateModId,
                        format!("Mod ID `{actual_id}` appears more than once in the batch"),
                    )
                    .with_mod(actual_id.clone())
                    .with_batch_index(index),
                ));
            }
            if self.registered.contains(&actual_id) {
                return Err(PlatformError::lifecycle(
                    LifecycleFailure::new(
                        LifecycleCode::AlreadyRegistered,
                        format!("Mod ID `{actual_id}` is already registered"),
                    )
                    .with_mod(actual_id.clone())
                    .with_batch_index(index),
                ));
            }
            if let Err(reason) = kernel_core::validate_type_id(&actual_id) {
                return Err(PlatformError::lifecycle(
                    LifecycleFailure::new(
                        LifecycleCode::InvalidModId,
                        format!("Mod ID `{actual_id}` is rejected by Kernel: {reason}"),
                    )
                    .with_mod(actual_id.clone())
                    .with_batch_index(index),
                ));
            }
            // Carry the System forward paired with its frozen identity.
            pending.push((actual_id, system));
        }

        // Phase 2: real registration with the frozen identity. The Kernel only
        // ever observes the pre-validated ID because `FrozenIdSystem::id()`
        // returns it verbatim. Each success is recorded immediately; a Kernel
        // rejection surfaces as a diagnosable failure.
        for (index, (frozen_id, system)) in pending.into_iter().enumerate() {
            let frozen = Box::new(FrozenIdSystem::new(frozen_id.clone(), system));
            kernel.register_system(frozen).map_err(|error| {
                PlatformError::lifecycle(
                    LifecycleFailure::new(
                        LifecycleCode::RegistrationRejected,
                        format!("Kernel rejected Mod ID `{frozen_id}`: {error}"),
                    )
                    .with_mod(frozen_id.clone())
                    .with_batch_index(index),
                )
            })?;
            self.registered.insert(frozen_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_failure_codes_are_stable_and_namespaced() {
        // The four pre-GK-P3-06 codes are preserved unchanged.
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
        // GK-P3-06 refined codes (frozen §3.2/§3.3).
        assert_eq!(
            ModFailureCode::FuelExceeded.as_str(),
            "platform.mod.resource.fuel_exceeded"
        );
        assert_eq!(
            ModFailureCode::MemoryOutOfBounds.as_str(),
            "platform.mod.resource.memory_out_of_bounds"
        );
        assert_eq!(
            ModFailureCode::StackExceeded.as_str(),
            "platform.mod.resource.stack_exceeded"
        );
        assert_eq!(
            ModFailureCode::QuotaExceeded.as_str(),
            "platform.mod.resource.quota_exceeded"
        );
        assert_eq!(
            ModFailureCode::MemoryLimitExceeded.as_str(),
            "platform.mod.resource.memory_limit_exceeded"
        );
        assert_eq!(
            ModFailureCode::StackLimitExceeded.as_str(),
            "platform.mod.resource.stack_limit_exceeded"
        );
        assert_eq!(
            ModFailureCode::LoadFailed.as_str(),
            "platform.mod.load_failed"
        );
        assert_eq!(
            ModFailureCode::InitializationFailed.as_str(),
            "platform.mod.initialization_failed"
        );
        assert_eq!(
            ModFailureCode::ApiInvocationFailed.as_str(),
            "platform.mod.api.invocation_failed"
        );
        assert_eq!(
            ModFailureCode::GuestTrapUnreachable.as_str(),
            "platform.mod.guest_trap.unreachable"
        );
        assert_eq!(
            ModFailureCode::GuestTrapIntegerOverflow.as_str(),
            "platform.mod.guest_trap.integer_overflow"
        );
        assert_eq!(
            ModFailureCode::GuestTrapIntegerDivideByZero.as_str(),
            "platform.mod.guest_trap.integer_divide_by_zero"
        );
        assert_eq!(
            ModFailureCode::GuestTrapInvalidConversion.as_str(),
            "platform.mod.guest_trap.invalid_conversion"
        );
        assert_eq!(
            ModFailureCode::GuestTrapInterrupted.as_str(),
            "platform.mod.guest_trap.interrupted"
        );
        assert_eq!(
            ModFailureCode::GuestTrapOther.as_str(),
            "platform.mod.guest_trap.other"
        );
        assert_eq!(
            ModFailureCode::GuestTrapUnknown.as_str(),
            "platform.mod.guest_trap.unknown"
        );
    }

    #[test]
    fn trap_variants_map_to_frozen_stable_codes() {
        assert_eq!(
            classify_runtime_trap(&Trap::OutOfFuel),
            "platform.mod.resource.fuel_exceeded"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::MemoryOutOfBounds),
            "platform.mod.resource.memory_out_of_bounds"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::StackOverflow),
            "platform.mod.resource.stack_exceeded"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::UnreachableCodeReached),
            "platform.mod.guest_trap.unreachable"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::IntegerOverflow),
            "platform.mod.guest_trap.integer_overflow"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::IntegerDivisionByZero),
            "platform.mod.guest_trap.integer_divide_by_zero"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::BadConversionToInteger),
            "platform.mod.guest_trap.invalid_conversion"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::Interrupt),
            "platform.mod.guest_trap.interrupted"
        );
        // The "other" group vs the truly-unknown group (frozen §3.2).
        assert_eq!(
            classify_runtime_trap(&Trap::TableOutOfBounds),
            "platform.mod.guest_trap.other"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::IndirectCallToNull),
            "platform.mod.guest_trap.other"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::BadSignature),
            "platform.mod.guest_trap.other"
        );
        assert_eq!(
            classify_runtime_trap(&Trap::NullReference),
            "platform.mod.guest_trap.other"
        );
        // A GC/component variant not in the "other" list must fall through to unknown.
        assert_eq!(
            classify_runtime_trap(&Trap::HeapMisaligned),
            "platform.mod.guest_trap.unknown"
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
        let error = match LoadedWasmMod::compile(&package, &grants, &engine) {
            Ok(_) => panic!("over-limit memory must be rejected before registration"),
            Err(error) => error,
        };
        // GK-P3-06: the instantiation-phase linear-memory limit reports its
        // frozen §3.3 C.2 code, distinct from the runtime OOB code.
        assert_eq!(
            error.fault_code(),
            "platform.mod.resource.memory_limit_exceeded"
        );
        let report = ModFaultReport::from_platform_error(&error);
        assert_eq!(report.category, FaultCategory::ResourceLimit);
        assert_eq!(report.phase, FaultPhase::Instantiation);
        assert_eq!(report.disposition, Disposition::Stop);
    }
}
