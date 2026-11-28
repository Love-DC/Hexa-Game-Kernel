//! Deterministic authoritative simulation Kernel with versioned P2 contracts.
//!
//! This crate intentionally has no dependency on Platform, Wasmtime, files, or networking.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SNAPSHOT_MAGIC: &str = "GAME_KERNEL_SNAPSHOT";
// Kernel-generated System commands are attributed under this reserved source prefix.
const SYSTEM_COMMAND_SOURCE_PREFIX: &str = "system/";
const SNAPSHOT_FORMAT_VERSION: u32 = 2;
const STATE_HASH_DOMAIN: &[u8] = b"game-kernel-world-state-v2\0";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Tick(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WorldId(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntityId {
    pub world: WorldId,
    pub index: u64,
    pub generation: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ComponentValue {
    Integer(i64),
    Boolean(bool),
    Text(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub id: EntityId,
    #[serde(with = "strict_component_map_serde")]
    components: BTreeMap<String, ComponentValue>,
}

impl Entity {
    pub fn component(&self, type_id: &str) -> Option<&ComponentValue> {
        self.components.get(type_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct World {
    id: WorldId,
    tick: Tick,
    next_entity_index: u64,
    #[serde(with = "entity_map_serde")]
    entities: BTreeMap<EntityId, Entity>,
    #[serde(with = "strict_component_map_serde")]
    resources: BTreeMap<String, ComponentValue>,
}

mod strict_component_map_serde {
    use std::collections::BTreeMap;
    use std::fmt;

    use serde::de::{MapAccess, Visitor};
    use serde::{Deserializer, Serialize, Serializer};

    use super::ComponentValue;

    pub fn serialize<S>(
        values: &BTreeMap<String, ComponentValue>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<String, ComponentValue>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ComponentMapVisitor;

        impl<'de> Visitor<'de> for ComponentMapVisitor {
            type Value = BTreeMap<String, ComponentValue>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map with unique component or resource IDs")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, ComponentValue>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate component or resource ID `{key}`"
                        )));
                    }
                }
                Ok(values)
            }
        }

        deserializer.deserialize_map(ComponentMapVisitor)
    }
}

mod entity_map_serde {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    use super::{Entity, EntityId};

    pub fn serialize<S>(
        entities: &BTreeMap<EntityId, Entity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        entities.values().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<EntityId, Entity>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entities = Vec::<Entity>::deserialize(deserializer)?;
        let mut result = BTreeMap::new();
        for entity in entities {
            if result.insert(entity.id, entity).is_some() {
                return Err(D::Error::custom("duplicate entity identity in snapshot"));
            }
        }
        Ok(result)
    }
}

impl World {
    pub fn new(id: WorldId) -> Self {
        Self {
            id,
            tick: Tick(0),
            next_entity_index: 0,
            entities: BTreeMap::new(),
            resources: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> WorldId {
        self.id
    }

    pub fn tick(&self) -> Tick {
        self.tick
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns entities in stable `EntityId` order without exposing mutable storage.
    pub fn entities(&self) -> impl ExactSizeIterator<Item = &Entity> {
        self.entities.values()
    }

    pub fn entity(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn resource(&self, key: &str) -> Option<&ComponentValue> {
        self.resources.get(key)
    }

    pub fn resource_i64(&self, key: &str) -> Option<i64> {
        match self.resource(key) {
            Some(ComponentValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    /// Sets initial state before handing ownership to `Kernel::new`.
    pub fn seed_resource(
        &mut self,
        key: impl Into<String>,
        value: ComponentValue,
    ) -> Result<(), KernelError> {
        let key = key.into();
        validate_type_id(&key).map_err(KernelError::InvalidInitialState)?;
        self.resources.insert(key, value);
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        let mut max_index = None;
        for (id, entity) in &self.entities {
            if id != &entity.id || id.world != self.id || id.generation != 0 {
                return Err("entity identity does not match the owning world".to_owned());
            }
            max_index = Some(max_index.map_or(id.index, |old: u64| old.max(id.index)));
            for type_id in entity.components.keys() {
                validate_type_id(type_id)?;
            }
        }
        if max_index.is_some_and(|index| self.next_entity_index <= index) {
            return Err("entity allocator would reuse an existing identity".to_owned());
        }
        for key in self.resources.keys() {
            validate_type_id(key)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CommandId {
    pub tick: Tick,
    pub source: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandEnvelope {
    pub id: CommandId,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    SetResource {
        key: String,
        value: ComponentValue,
    },
    SpawnEntity {
        components: BTreeMap<String, ComponentValue>,
    },
    DespawnEntity {
        entity: EntityId,
    },
    SetComponent {
        entity: EntityId,
        type_id: String,
        value: ComponentValue,
    },
    RemoveComponent {
        entity: EntityId,
        type_id: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CommandRejectionCode {
    #[serde(rename = "command.duplicate_id")]
    DuplicateCommandId,
    #[serde(rename = "command.wrong_target_tick")]
    WrongTargetTick,
    #[serde(rename = "command.invalid_source_id")]
    InvalidSourceId,
    #[serde(rename = "command.non_increasing_sequence")]
    NonIncreasingSequence,
    #[serde(rename = "command.invalid_type_id")]
    InvalidTypeId,
    #[serde(rename = "world.entity_id_exhausted")]
    EntityIdExhausted,
    #[serde(rename = "world.cross_world_entity")]
    CrossWorldEntity,
    #[serde(rename = "world.invalid_entity_generation")]
    InvalidEntityGeneration,
    #[serde(rename = "world.missing_entity")]
    MissingEntity,
    #[serde(rename = "world.missing_component")]
    MissingComponent,
}

impl CommandRejectionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateCommandId => "command.duplicate_id",
            Self::WrongTargetTick => "command.wrong_target_tick",
            Self::InvalidSourceId => "command.invalid_source_id",
            Self::NonIncreasingSequence => "command.non_increasing_sequence",
            Self::InvalidTypeId => "command.invalid_type_id",
            Self::EntityIdExhausted => "world.entity_id_exhausted",
            Self::CrossWorldEntity => "world.cross_world_entity",
            Self::InvalidEntityGeneration => "world.invalid_entity_generation",
            Self::MissingEntity => "world.missing_entity",
            Self::MissingComponent => "world.missing_component",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandRejection {
    pub id: CommandId,
    pub code: CommandRejectionCode,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    ResourceSet { key: String, value: ComponentValue },
    EntitySpawned { entity: EntityId },
    EntityDespawned { entity: EntityId },
    ComponentSet { entity: EntityId, type_id: String },
    ComponentRemoved { entity: EntityId, type_id: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventRecord {
    pub tick: Tick,
    pub sequence: u64,
    pub source: String,
    pub event: EventKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateHash(pub [u8; 32]);

impl StateHash {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepInput {
    pub commands: Vec<CommandEnvelope>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepOutput {
    /// Tick after the successful step.
    pub tick: Tick,
    pub committed_commands: Vec<CommandId>,
    pub rejected_commands: Vec<CommandRejection>,
    pub events: Vec<EventRecord>,
    pub state_hash: StateHash,
}

/// Versioned, serializable diagnostic returned when a System cannot produce a Tick result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, Error)]
#[serde(deny_unknown_fields)]
#[error("system `{system}` failed ({code}): {message}")]
pub struct SystemError {
    pub format_version: u32,
    /// Stable namespaced code. Diagnostic text is not a machine-readable contract.
    pub code: String,
    pub system: String,
    pub tick: Tick,
    /// Present for content-addressed systems such as Mods.
    pub source_digest: Option<String>,
    pub message: String,
}

impl SystemError {
    pub fn new(
        system: impl Into<String>,
        tick: Tick,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            format_version: 1,
            code: code.into(),
            system: system.into(),
            tick,
            source_digest: None,
            message: message.into(),
        }
    }

    pub fn with_source_digest(mut self, digest: impl Into<String>) -> Self {
        self.source_digest = Some(digest.into());
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelErrorCode {
    InvalidInitialState,
    TickOverflow,
    Faulted,
    InvalidSystemId,
    DuplicateSystemId,
    SystemFailed,
    InvalidSystemCommand,
    Serialization,
    UnsupportedSnapshotVersion,
    InvalidSnapshotMagic,
    SnapshotHashMismatch,
    InvalidSnapshotWorld,
}

impl KernelErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInitialState => "kernel.invalid_initial_state",
            Self::TickOverflow => "kernel.tick_overflow",
            Self::Faulted => "kernel.faulted",
            Self::InvalidSystemId => "kernel.invalid_system_id",
            Self::DuplicateSystemId => "kernel.duplicate_system_id",
            Self::SystemFailed => "kernel.system_failed",
            Self::InvalidSystemCommand => "kernel.invalid_system_command",
            Self::Serialization => "kernel.serialization",
            Self::UnsupportedSnapshotVersion => "kernel.unsupported_snapshot_version",
            Self::InvalidSnapshotMagic => "kernel.invalid_snapshot_magic",
            Self::SnapshotHashMismatch => "kernel.snapshot_hash_mismatch",
            Self::InvalidSnapshotWorld => "kernel.invalid_snapshot_world",
        }
    }
}

pub trait System: Send + Sync {
    /// Returns the System's registration identity.
    ///
    /// The Kernel calls this once when registration is attempted. A successful
    /// registration freezes that value for duplicate detection, manifest reads,
    /// execution ordering, Kernel-generated command diagnostics, and command
    /// attribution. Changing the returned value afterward has no effect on the
    /// registered identity.
    fn id(&self) -> &str;
    fn execute(&self, world: &World, tick: Tick) -> Result<Vec<Command>, SystemError>;
}

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("invalid initial world state: {0}")]
    InvalidInitialState(String),
    #[error("tick overflow at {0}")]
    TickOverflow(u64),
    #[error("kernel is faulted: {0}")]
    Faulted(String),
    #[error("invalid system ID `{0}`")]
    InvalidSystemId(String),
    #[error("duplicate system ID `{0}`")]
    DuplicateSystemId(String),
    #[error("system execution failed: {0}")]
    SystemFailed(#[from] SystemError),
    #[error("system `{system}` produced an invalid command: {reason}")]
    InvalidSystemCommand { system: String, reason: String },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("unsupported snapshot format version {0}")]
    UnsupportedSnapshotVersion(u32),
    #[error("invalid snapshot magic `{0}`")]
    InvalidSnapshotMagic(String),
    #[error("snapshot state hash does not match its state")]
    SnapshotHashMismatch,
    #[error("invalid snapshot world: {0}")]
    InvalidSnapshotWorld(String),
}

impl KernelError {
    pub const fn code(&self) -> KernelErrorCode {
        match self {
            Self::InvalidInitialState(_) => KernelErrorCode::InvalidInitialState,
            Self::TickOverflow(_) => KernelErrorCode::TickOverflow,
            Self::Faulted(_) => KernelErrorCode::Faulted,
            Self::InvalidSystemId(_) => KernelErrorCode::InvalidSystemId,
            Self::DuplicateSystemId(_) => KernelErrorCode::DuplicateSystemId,
            Self::SystemFailed(_) => KernelErrorCode::SystemFailed,
            Self::InvalidSystemCommand { .. } => KernelErrorCode::InvalidSystemCommand,
            Self::Serialization(_) => KernelErrorCode::Serialization,
            Self::UnsupportedSnapshotVersion(_) => KernelErrorCode::UnsupportedSnapshotVersion,
            Self::InvalidSnapshotMagic(_) => KernelErrorCode::InvalidSnapshotMagic,
            Self::SnapshotHashMismatch => KernelErrorCode::SnapshotHashMismatch,
            Self::InvalidSnapshotWorld(_) => KernelErrorCode::InvalidSnapshotWorld,
        }
    }
}

#[derive(Debug)]
struct Rejection {
    code: CommandRejectionCode,
    reason: String,
}

impl Rejection {
    fn new(code: CommandRejectionCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

pub struct Kernel {
    world: World,
    systems: Vec<RegisteredSystem>,
    fault: Option<String>,
}

struct RegisteredSystem {
    id: String,
    system: Box<dyn System>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotEnvelope {
    magic: String,
    format_version: u32,
    world: World,
    state_hash: StateHash,
}

impl Kernel {
    pub fn new(world: World) -> Result<Self, KernelError> {
        world.validate().map_err(KernelError::InvalidInitialState)?;
        Ok(Self {
            world,
            systems: Vec::new(),
            fault: None,
        })
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    /// Returns the registered System IDs in execution order (stable
    /// lexicographic order by System ID).
    ///
    /// Read-only and side-effect free; the returned sequence matches the set
    /// of systems actually registered on this Kernel (C-1). IDs are captured
    /// at successful registration and never re-read from the System instance.
    pub fn registered_system_ids(&self) -> impl ExactSizeIterator<Item = &str> {
        self.systems.iter().map(|registered| registered.id.as_str())
    }

    pub fn register_system(&mut self, system: Box<dyn System>) -> Result<(), KernelError> {
        self.register_systems(vec![system])
    }

    /// Atomically registers a batch of Systems.
    ///
    /// All IDs are captured and validated before the Kernel's registration set
    /// changes. If any ID is invalid or duplicated (within the batch or against
    /// an existing registration), the method returns an error without registering
    /// any System from this batch. Successful registrations are ordered by their
    /// frozen IDs, matching repeated calls to `register_system` for a valid set.
    pub fn register_systems(&mut self, systems: Vec<Box<dyn System>>) -> Result<(), KernelError> {
        let mut pending = Vec::with_capacity(systems.len());
        let mut batch_ids = BTreeSet::new();

        for system in systems {
            let id = system.id().to_owned();
            validate_type_id(&id).map_err(|_| KernelError::InvalidSystemId(id.clone()))?;
            if self.systems.iter().any(|registered| registered.id == id)
                || !batch_ids.insert(id.clone())
            {
                return Err(KernelError::DuplicateSystemId(id));
            }
            pending.push(RegisteredSystem { id, system });
        }

        self.systems.extend(pending);
        self.systems.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(())
    }

    pub fn step(&mut self, input: StepInput) -> Result<StepOutput, KernelError> {
        if let Some(reason) = &self.fault {
            return Err(KernelError::Faulted(reason.clone()));
        }

        let current_tick = self.world.tick;
        let next_tick = current_tick
            .0
            .checked_add(1)
            .map(Tick)
            .ok_or(KernelError::TickOverflow(current_tick.0))?;
        let mut staged = self.world.clone();
        let mut committed = Vec::new();
        let mut rejected = Vec::new();
        let mut events = Vec::new();
        let mut seen_ids = BTreeSet::new();
        let mut last_sequence_by_source = BTreeMap::<String, u64>::new();

        let mut external = input.commands;
        external.sort_by(|left, right| {
            (&left.id.source, left.id.sequence).cmp(&(&right.id.source, right.id.sequence))
        });

        for envelope in external {
            if !seen_ids.insert(envelope.id.clone()) {
                rejected.push(reject(
                    &envelope,
                    CommandRejectionCode::DuplicateCommandId,
                    "duplicate command ID",
                ));
                continue;
            }
            if envelope.id.tick != current_tick {
                rejected.push(reject(
                    &envelope,
                    CommandRejectionCode::WrongTargetTick,
                    "command targets the wrong Tick",
                ));
                continue;
            }
            if validate_type_id(&envelope.id.source).is_err() {
                rejected.push(reject(
                    &envelope,
                    CommandRejectionCode::InvalidSourceId,
                    "invalid command source ID",
                ));
                continue;
            }

            // The Kernel owns this source namespace for commands emitted by Systems.
            if envelope.id.source.starts_with(SYSTEM_COMMAND_SOURCE_PREFIX) {
                rejected.push(reject(
                    &envelope,
                    CommandRejectionCode::InvalidSourceId,
                    "command source uses the Kernel-reserved system namespace",
                ));
                continue;
            }
            if last_sequence_by_source
                .get(&envelope.id.source)
                .is_some_and(|last| envelope.id.sequence <= *last)
            {
                rejected.push(reject(
                    &envelope,
                    CommandRejectionCode::NonIncreasingSequence,
                    "sequence is not strictly increasing",
                ));
                continue;
            }
            last_sequence_by_source.insert(envelope.id.source.clone(), envelope.id.sequence);

            match apply_command(&mut staged, &envelope.command) {
                Ok(event) => {
                    committed.push(envelope.id.clone());
                    if let Some(event) = event {
                        events.push(EventRecord {
                            tick: current_tick,
                            sequence: events.len() as u64,
                            source: envelope.id.source,
                            event,
                        });
                    }
                }
                Err(reason) => rejected.push(reject(&envelope, reason.code, &reason.reason)),
            }
        }

        // All systems read the same world after external commands. Their outputs are
        // applied later in stable SystemId order, never through mutable World access.
        let mut system_outputs = Vec::with_capacity(self.systems.len());
        for registered in &self.systems {
            match registered.system.execute(&staged, current_tick) {
                Ok(commands) => system_outputs.push((registered.id.clone(), commands)),
                Err(mut error) => {
                    // The Kernel owns the registration identity and executing Tick.
                    // Do not trust a System's self-reported diagnostic coordinates.
                    error.system = registered.id.clone();
                    error.tick = current_tick;
                    self.fault = Some(error.to_string());
                    return Err(KernelError::SystemFailed(error));
                }
            }
        }

        for (system_id, commands) in system_outputs {
            let source = format!("system/{system_id}");
            let mut system_staged = staged.clone();
            let mut system_events = Vec::new();
            let mut system_ids = Vec::with_capacity(commands.len());

            for (sequence, command) in commands.into_iter().enumerate() {
                let sequence = u64::try_from(sequence)
                    .map_err(|_| KernelError::TickOverflow(current_tick.0))?;
                let id = CommandId {
                    tick: current_tick,
                    source: source.clone(),
                    sequence,
                };
                match apply_command(&mut system_staged, &command) {
                    Ok(event) => {
                        system_ids.push(id);
                        if let Some(event) = event {
                            system_events.push(event);
                        }
                    }
                    Err(reason) => {
                        let error = KernelError::InvalidSystemCommand {
                            system: system_id.clone(),
                            reason: format!(
                                "tick={} command_source={} command_sequence={}: {}: {}",
                                current_tick.0,
                                source,
                                sequence,
                                reason.code.as_str(),
                                reason.reason
                            ),
                        };
                        self.fault = Some(error.to_string());
                        return Err(error);
                    }
                }
            }

            staged = system_staged;
            committed.extend(system_ids);
            for event in system_events {
                events.push(EventRecord {
                    tick: current_tick,
                    sequence: events.len() as u64,
                    source: source.clone(),
                    event,
                });
            }
        }

        staged.tick = next_tick;
        let state_hash = state_hash(&staged)?;
        self.world = staged;

        Ok(StepOutput {
            tick: next_tick,
            committed_commands: committed,
            rejected_commands: rejected,
            events,
            state_hash,
        })
    }

    pub fn snapshot(&self) -> Result<Vec<u8>, KernelError> {
        let envelope = SnapshotEnvelope {
            magic: SNAPSHOT_MAGIC.to_owned(),
            format_version: SNAPSHOT_FORMAT_VERSION,
            world: self.world.clone(),
            state_hash: state_hash(&self.world)?,
        };
        serde_json::to_vec(&envelope).map_err(|error| KernelError::Serialization(error.to_string()))
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, KernelError> {
        let envelope: SnapshotEnvelope = serde_json::from_slice(bytes)
            .map_err(|error| KernelError::Serialization(error.to_string()))?;
        if envelope.magic != SNAPSHOT_MAGIC {
            return Err(KernelError::InvalidSnapshotMagic(envelope.magic));
        }
        if envelope.format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(KernelError::UnsupportedSnapshotVersion(
                envelope.format_version,
            ));
        }
        envelope
            .world
            .validate()
            .map_err(KernelError::InvalidSnapshotWorld)?;
        if state_hash(&envelope.world)? != envelope.state_hash {
            return Err(KernelError::SnapshotHashMismatch);
        }
        Ok(Self {
            world: envelope.world,
            systems: Vec::new(),
            fault: None,
        })
    }
}

fn reject(
    envelope: &CommandEnvelope,
    code: CommandRejectionCode,
    reason: &str,
) -> CommandRejection {
    CommandRejection {
        id: envelope.id.clone(),
        code,
        reason: reason.to_owned(),
    }
}

fn apply_command(world: &mut World, command: &Command) -> Result<Option<EventKind>, Rejection> {
    match command {
        Command::SetResource { key, value } => {
            validate_type_id(key)
                .map_err(|reason| Rejection::new(CommandRejectionCode::InvalidTypeId, reason))?;
            world.resources.insert(key.clone(), value.clone());
            Ok(Some(EventKind::ResourceSet {
                key: key.clone(),
                value: value.clone(),
            }))
        }
        Command::SpawnEntity { components } => {
            for type_id in components.keys() {
                validate_type_id(type_id).map_err(|reason| {
                    Rejection::new(CommandRejectionCode::InvalidTypeId, reason)
                })?;
            }
            let index = world.next_entity_index;
            let next = index.checked_add(1).ok_or_else(|| {
                Rejection::new(
                    CommandRejectionCode::EntityIdExhausted,
                    "entity ID space exhausted",
                )
            })?;
            let id = EntityId {
                world: world.id,
                index,
                generation: 0,
            };
            world.next_entity_index = next;
            world.entities.insert(
                id,
                Entity {
                    id,
                    components: components.clone(),
                },
            );
            Ok(Some(EventKind::EntitySpawned { entity: id }))
        }
        Command::DespawnEntity { entity } => {
            validate_entity_target(world, *entity)?;
            world.entities.remove(entity).ok_or_else(|| {
                Rejection::new(
                    CommandRejectionCode::MissingEntity,
                    "target entity does not exist",
                )
            })?;
            Ok(Some(EventKind::EntityDespawned { entity: *entity }))
        }
        Command::SetComponent {
            entity,
            type_id,
            value,
        } => {
            validate_type_id(type_id)
                .map_err(|reason| Rejection::new(CommandRejectionCode::InvalidTypeId, reason))?;
            validate_entity_target(world, *entity)?;
            let target = world.entities.get_mut(entity).ok_or_else(|| {
                Rejection::new(
                    CommandRejectionCode::MissingEntity,
                    "target entity does not exist",
                )
            })?;
            target.components.insert(type_id.clone(), value.clone());
            Ok(Some(EventKind::ComponentSet {
                entity: *entity,
                type_id: type_id.clone(),
            }))
        }
        Command::RemoveComponent { entity, type_id } => {
            validate_type_id(type_id)
                .map_err(|reason| Rejection::new(CommandRejectionCode::InvalidTypeId, reason))?;
            validate_entity_target(world, *entity)?;
            let target = world.entities.get_mut(entity).ok_or_else(|| {
                Rejection::new(
                    CommandRejectionCode::MissingEntity,
                    "target entity does not exist",
                )
            })?;
            if target.components.remove(type_id).is_none() {
                return Err(Rejection::new(
                    CommandRejectionCode::MissingComponent,
                    "component does not exist",
                ));
            }
            Ok(Some(EventKind::ComponentRemoved {
                entity: *entity,
                type_id: type_id.clone(),
            }))
        }
    }
}

fn validate_entity_target(world: &World, entity: EntityId) -> Result<(), Rejection> {
    if entity.world != world.id {
        return Err(Rejection::new(
            CommandRejectionCode::CrossWorldEntity,
            "entity belongs to another world",
        ));
    }
    if entity.generation != 0 {
        return Err(Rejection::new(
            CommandRejectionCode::InvalidEntityGeneration,
            "unknown entity generation",
        ));
    }
    Ok(())
}

pub fn validate_type_id(value: &str) -> Result<(), String> {
    let valid_chars = value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '-' | '_' | '/')
    });
    let valid_shape = !value.is_empty()
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("//");
    if value.len() > 128 || !valid_chars || !valid_shape {
        return Err(format!("invalid stable type/source ID `{value}`"));
    }
    Ok(())
}

fn state_hash(world: &World) -> Result<StateHash, KernelError> {
    let mut hasher = Sha256::new();
    hasher.update(STATE_HASH_DOMAIN);
    hash_u64(&mut hasher, world.id.0);
    hash_u64(&mut hasher, world.tick.0);
    hash_u64(&mut hasher, world.next_entity_index);
    hash_len(&mut hasher, world.entities.len())?;
    for entity in world.entities.values() {
        hash_entity_id(&mut hasher, entity.id);
        hash_len(&mut hasher, entity.components.len())?;
        for (type_id, value) in &entity.components {
            hash_string(&mut hasher, type_id)?;
            hash_component_value(&mut hasher, value)?;
        }
    }
    hash_len(&mut hasher, world.resources.len())?;
    for (key, value) in &world.resources {
        hash_string(&mut hasher, key)?;
        hash_component_value(&mut hasher, value)?;
    }
    let digest = hasher.finalize();
    let mut result = [0_u8; 32];
    result.copy_from_slice(&digest);
    Ok(StateHash(result))
}

fn hash_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_le_bytes());
}

fn hash_len(hasher: &mut Sha256, value: usize) -> Result<(), KernelError> {
    let value = u64::try_from(value)
        .map_err(|_| KernelError::Serialization("collection length exceeds u64".to_owned()))?;
    hash_u64(hasher, value);
    Ok(())
}

fn hash_string(hasher: &mut Sha256, value: &str) -> Result<(), KernelError> {
    hash_len(hasher, value.len())?;
    hasher.update(value.as_bytes());
    Ok(())
}

fn hash_entity_id(hasher: &mut Sha256, id: EntityId) {
    hash_u64(hasher, id.world.0);
    hash_u64(hasher, id.index);
    hasher.update(id.generation.to_le_bytes());
}

fn hash_component_value(hasher: &mut Sha256, value: &ComponentValue) -> Result<(), KernelError> {
    match value {
        ComponentValue::Integer(value) => {
            hasher.update([1]);
            hasher.update(value.to_le_bytes());
        }
        ComponentValue::Boolean(value) => {
            hasher.update([2, u8::from(*value)]);
        }
        ComponentValue::Text(value) => {
            hasher.update([3]);
            hash_string(hasher, value)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(sequence: u64, command: Command) -> CommandEnvelope {
        CommandEnvelope {
            id: CommandId {
                tick: Tick(0),
                source: "external/test".to_owned(),
                sequence,
            },
            command,
        }
    }

    fn seeded_kernel(value: i64) -> Kernel {
        let mut world = World::new(WorldId(7));
        world
            .seed_resource("org.game/counter", ComponentValue::Integer(value))
            .unwrap();
        Kernel::new(world).unwrap()
    }

    #[test]
    fn successful_step_commits_command_and_advances_once() {
        let mut kernel = seeded_kernel(1);
        let output = kernel
            .step(StepInput {
                commands: vec![command(
                    0,
                    Command::SetResource {
                        key: "org.game/counter".to_owned(),
                        value: ComponentValue::Integer(2),
                    },
                )],
            })
            .unwrap();

        assert_eq!(output.tick, Tick(1));
        assert_eq!(kernel.world().tick(), Tick(1));
        assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(2));
        assert_eq!(output.committed_commands.len(), 1);
        assert_eq!(output.events.len(), 1);
    }

    #[test]
    fn rejected_external_command_does_not_block_later_valid_command() {
        let mut kernel = seeded_kernel(1);
        let output = kernel
            .step(StepInput {
                commands: vec![
                    command(
                        0,
                        Command::SetComponent {
                            entity: EntityId {
                                world: WorldId(7),
                                index: 99,
                                generation: 0,
                            },
                            type_id: "org.game/health".to_owned(),
                            value: ComponentValue::Integer(10),
                        },
                    ),
                    command(
                        1,
                        Command::SetResource {
                            key: "org.game/counter".to_owned(),
                            value: ComponentValue::Integer(3),
                        },
                    ),
                ],
            })
            .unwrap();

        assert_eq!(output.rejected_commands.len(), 1);
        assert_eq!(
            output.rejected_commands[0].code,
            CommandRejectionCode::MissingEntity
        );
        assert_eq!(output.committed_commands.len(), 1);
        assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(3));
    }

    #[test]
    fn entity_identity_errors_have_distinct_stable_codes() {
        let mut kernel = seeded_kernel(0);
        let output = kernel
            .step(StepInput {
                commands: vec![
                    command(
                        0,
                        Command::DespawnEntity {
                            entity: EntityId {
                                world: WorldId(8),
                                index: 0,
                                generation: 0,
                            },
                        },
                    ),
                    command(
                        1,
                        Command::DespawnEntity {
                            entity: EntityId {
                                world: WorldId(7),
                                index: 0,
                                generation: 1,
                            },
                        },
                    ),
                    command(
                        2,
                        Command::DespawnEntity {
                            entity: EntityId {
                                world: WorldId(7),
                                index: 0,
                                generation: 0,
                            },
                        },
                    ),
                ],
            })
            .unwrap();

        assert_eq!(
            output
                .rejected_commands
                .iter()
                .map(|rejection| rejection.code)
                .collect::<Vec<_>>(),
            [
                CommandRejectionCode::CrossWorldEntity,
                CommandRejectionCode::InvalidEntityGeneration,
                CommandRejectionCode::MissingEntity
            ]
        );
        assert_eq!(
            output.rejected_commands[0].code.as_str(),
            "world.cross_world_entity"
        );
        let serialized = serde_json::to_value(&output.rejected_commands[0]).unwrap();
        assert_eq!(serialized["code"], "world.cross_world_entity");
    }

    #[test]
    fn entity_allocator_overflow_is_a_stable_command_rejection() {
        let mut world = World::new(WorldId(7));
        world.next_entity_index = u64::MAX;
        let mut kernel = Kernel::new(world).unwrap();
        let output = kernel
            .step(StepInput {
                commands: vec![command(
                    0,
                    Command::SpawnEntity {
                        components: BTreeMap::new(),
                    },
                )],
            })
            .unwrap();

        assert_eq!(
            output.rejected_commands[0].code,
            CommandRejectionCode::EntityIdExhausted
        );
        assert_eq!(kernel.world().entity_count(), 0);
    }

    #[test]
    fn duplicate_command_id_is_rejected() {
        let mut kernel = seeded_kernel(0);
        let envelope = command(
            0,
            Command::SetResource {
                key: "org.game/counter".to_owned(),
                value: ComponentValue::Integer(1),
            },
        );
        let output = kernel
            .step(StepInput {
                commands: vec![envelope.clone(), envelope],
            })
            .unwrap();

        assert_eq!(output.committed_commands.len(), 1);
        assert_eq!(output.rejected_commands.len(), 1);
        assert_eq!(
            output.rejected_commands[0].code,
            CommandRejectionCode::DuplicateCommandId
        );
    }

    #[test]
    fn spawn_then_set_component_is_visible_in_one_batch() {
        let mut kernel = seeded_kernel(0);
        let new_entity = EntityId {
            world: WorldId(7),
            index: 0,
            generation: 0,
        };
        let output = kernel
            .step(StepInput {
                commands: vec![
                    command(
                        0,
                        Command::SpawnEntity {
                            components: BTreeMap::new(),
                        },
                    ),
                    command(
                        1,
                        Command::SetComponent {
                            entity: new_entity,
                            type_id: "org.game/health".to_owned(),
                            value: ComponentValue::Integer(10),
                        },
                    ),
                ],
            })
            .unwrap();

        assert_eq!(output.committed_commands.len(), 2);
        assert_eq!(kernel.world().entity_count(), 1);
        assert_eq!(
            kernel
                .world()
                .entity(new_entity)
                .unwrap()
                .component("org.game/health"),
            Some(&ComponentValue::Integer(10))
        );
    }

    #[test]
    fn entities_are_exposed_in_stable_id_order() {
        let mut kernel = seeded_kernel(0);
        kernel
            .step(StepInput {
                commands: vec![
                    command(
                        0,
                        Command::SpawnEntity {
                            components: BTreeMap::new(),
                        },
                    ),
                    command(
                        1,
                        Command::SpawnEntity {
                            components: BTreeMap::new(),
                        },
                    ),
                ],
            })
            .unwrap();

        let ids = kernel
            .world()
            .entities()
            .map(|entity| entity.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.iter().map(|id| id.index).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn snapshot_restore_preserves_state_and_hash() {
        let mut kernel = seeded_kernel(7);
        let first = kernel.step(StepInput::default()).unwrap();
        let snapshot = kernel.snapshot().unwrap();
        let restored = Kernel::from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.world().tick(), Tick(1));
        assert_eq!(restored.world().resource_i64("org.game/counter"), Some(7));
        assert_eq!(state_hash(restored.world()).unwrap(), first.state_hash);
    }

    #[test]
    fn snapshot_v2_declares_its_magic_and_rejects_v1() {
        let kernel = seeded_kernel(7);
        let snapshot = kernel.snapshot().unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        assert_eq!(envelope["magic"], SNAPSHOT_MAGIC);
        assert_eq!(envelope["format_version"], SNAPSHOT_FORMAT_VERSION);

        envelope["format_version"] = serde_json::json!(1);
        let old_snapshot = serde_json::to_vec(&envelope).unwrap();
        assert!(matches!(
            Kernel::from_snapshot(&old_snapshot),
            Err(KernelError::UnsupportedSnapshotVersion(1))
        ));
    }

    #[test]
    fn snapshot_rejects_wrong_magic_and_state_hash() {
        let kernel = seeded_kernel(7);
        let snapshot = kernel.snapshot().unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();

        envelope["magic"] = serde_json::json!("NOT_A_KERNEL_SNAPSHOT");
        let wrong_magic = serde_json::to_vec(&envelope).unwrap();
        assert!(matches!(
            Kernel::from_snapshot(&wrong_magic),
            Err(KernelError::InvalidSnapshotMagic(_))
        ));

        envelope["magic"] = serde_json::json!(SNAPSHOT_MAGIC);
        envelope["state_hash"] = serde_json::json!(vec![0; 32]);
        let wrong_hash = serde_json::to_vec(&envelope).unwrap();
        assert!(matches!(
            Kernel::from_snapshot(&wrong_hash),
            Err(KernelError::SnapshotHashMismatch)
        ));
    }

    #[test]
    fn snapshot_rejects_unknown_nested_fields_and_duplicate_map_keys() {
        let kernel = seeded_kernel(7);
        let snapshot = kernel.snapshot().unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        envelope["world"]["unknown"] = serde_json::json!(true);
        let with_unknown_field = serde_json::to_vec(&envelope).unwrap();
        assert!(matches!(
            Kernel::from_snapshot(&with_unknown_field),
            Err(KernelError::Serialization(_))
        ));

        let duplicate_resource_key = br#"{
            "magic":"GAME_KERNEL_SNAPSHOT",
            "format_version":2,
            "world":{"id":7,"tick":0,"next_entity_index":0,"entities":[],"resources":{
                "org.game/counter":{"kind":"integer","value":1},
                "org.game/counter":{"kind":"integer","value":2}
            }},
            "state_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        }"#;
        assert!(matches!(
            Kernel::from_snapshot(duplicate_resource_key),
            Err(KernelError::Serialization(_))
        ));

        let duplicate_component_key = br#"{
            "magic":"GAME_KERNEL_SNAPSHOT",
            "format_version":2,
            "world":{"id":7,"tick":0,"next_entity_index":1,"entities":[{
                "id":{"world":7,"index":0,"generation":0},
                "components":{
                    "org.game/health":{"kind":"integer","value":1},
                    "org.game/health":{"kind":"integer","value":2}
                }
            }],"resources":{}},
            "state_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        }"#;
        assert!(matches!(
            Kernel::from_snapshot(duplicate_component_key),
            Err(KernelError::Serialization(_))
        ));

        let duplicate_entity_id = br#"{
            "magic":"GAME_KERNEL_SNAPSHOT",
            "format_version":2,
            "world":{"id":7,"tick":0,"next_entity_index":1,"entities":[
                {"id":{"world":7,"index":0,"generation":0},"components":{}},
                {"id":{"world":7,"index":0,"generation":0},"components":{}}
            ],"resources":{}},
            "state_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        }"#;
        assert!(matches!(
            Kernel::from_snapshot(duplicate_entity_id),
            Err(KernelError::Serialization(_))
        ));
        assert!(matches!(
            Kernel::from_snapshot(br#"{"magic":"GAME_KERNEL_SNAPSHOT""#),
            Err(KernelError::Serialization(_))
        ));
    }

    #[test]
    fn snapshot_rejects_entity_that_does_not_belong_to_its_world() {
        let invalid_world = br#"{
            "magic":"GAME_KERNEL_SNAPSHOT",
            "format_version":2,
            "world":{"id":7,"tick":0,"next_entity_index":1,"entities":[{
                "id":{"world":8,"index":0,"generation":0},"components":{}
            }],"resources":{}},
            "state_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        }"#;
        assert!(matches!(
            Kernel::from_snapshot(invalid_world),
            Err(KernelError::InvalidSnapshotWorld(_))
        ));
    }

    #[test]
    fn state_hash_uses_canonical_v2_encoding() {
        let mut first = World::new(WorldId(7));
        first.tick = Tick(4);
        first.next_entity_index = 6;
        first
            .seed_resource("org.game/counter", ComponentValue::Integer(9))
            .unwrap();
        first
            .seed_resource("org.game/name", ComponentValue::Text("kernel".to_owned()))
            .unwrap();
        first
            .seed_resource("org.game/ready", ComponentValue::Boolean(true))
            .unwrap();
        let entity_id = EntityId {
            world: WorldId(7),
            index: 5,
            generation: 0,
        };
        first.entities.insert(
            entity_id,
            Entity {
                id: entity_id,
                components: BTreeMap::from([
                    ("org.game/count".to_owned(), ComponentValue::Integer(-3)),
                    (
                        "org.game/enabled".to_owned(),
                        ComponentValue::Boolean(false),
                    ),
                    (
                        "org.game/label".to_owned(),
                        ComponentValue::Text("unit".to_owned()),
                    ),
                ]),
            },
        );
        let mut second = World::new(WorldId(7));
        second.tick = Tick(4);
        second.next_entity_index = 6;
        second
            .resources
            .insert("org.game/ready".to_owned(), ComponentValue::Boolean(true));
        second
            .seed_resource("org.game/name", ComponentValue::Text("kernel".to_owned()))
            .unwrap();
        second
            .seed_resource("org.game/counter", ComponentValue::Integer(9))
            .unwrap();
        let components = BTreeMap::from([
            (
                "org.game/label".to_owned(),
                ComponentValue::Text("unit".to_owned()),
            ),
            (
                "org.game/enabled".to_owned(),
                ComponentValue::Boolean(false),
            ),
            ("org.game/count".to_owned(), ComponentValue::Integer(-3)),
        ]);
        second.entities.insert(
            entity_id,
            Entity {
                id: entity_id,
                components,
            },
        );

        assert_eq!(state_hash(&first).unwrap(), state_hash(&second).unwrap());
        assert_eq!(
            state_hash(&first).unwrap().to_hex(),
            "ed40c73c2133356f32f4ae4f72834c1e74b1aacae74a2033b1f4976aaa77b5e0"
        );
    }

    struct FailingSystem;

    impl System for FailingSystem {
        fn id(&self) -> &str {
            "org.game/failing"
        }

        fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
            Err(SystemError::new(
                self.id(),
                _tick,
                "kernel.test_failure",
                "fixture failure",
            ))
        }
    }

    #[test]
    fn failed_system_rolls_back_tick_and_faults_kernel() {
        let mut kernel = seeded_kernel(1);
        kernel.register_system(Box::new(FailingSystem)).unwrap();
        let result = kernel.step(StepInput {
            commands: vec![command(
                0,
                Command::SetResource {
                    key: "org.game/counter".to_owned(),
                    value: ComponentValue::Integer(99),
                },
            )],
        });

        assert!(matches!(&result, Err(KernelError::SystemFailed(_))));
        assert_eq!(
            result.as_ref().unwrap_err().code().as_str(),
            "kernel.system_failed"
        );
        assert_eq!(kernel.world().tick(), Tick(0));
        assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(1));
        let faulted = kernel.step(StepInput::default());
        assert!(matches!(&faulted, Err(KernelError::Faulted(_))));
        assert_eq!(faulted.unwrap_err().code().as_str(), "kernel.faulted");
    }
}
