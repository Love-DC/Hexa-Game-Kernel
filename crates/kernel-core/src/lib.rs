//! Minimal authoritative simulation Kernel for the P1 vertical slice.
//!
//! This crate intentionally has no dependency on Platform, Wasmtime, files, or networking.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SNAPSHOT_FORMAT_VERSION: u32 = 1;
const STATE_HASH_DOMAIN: &[u8] = b"game-kernel-world-state-v1\0";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Tick(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WorldId(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
pub struct Entity {
    pub id: EntityId,
    components: BTreeMap<String, ComponentValue>,
}

impl Entity {
    pub fn component(&self, type_id: &str) -> Option<&ComponentValue> {
        self.components.get(type_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct World {
    id: WorldId,
    tick: Tick,
    next_entity_index: u64,
    #[serde(with = "entity_map_serde")]
    entities: BTreeMap<EntityId, Entity>,
    resources: BTreeMap<String, ComponentValue>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandRejection {
    pub id: CommandId,
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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("system `{system}` failed: {message}")]
pub struct SystemError {
    pub system: String,
    pub message: String,
}

pub trait System: Send + Sync {
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
    #[error("snapshot state hash does not match its state")]
    SnapshotHashMismatch,
    #[error("invalid snapshot world: {0}")]
    InvalidSnapshotWorld(String),
}

#[derive(Debug, Error)]
#[error("{0}")]
struct Rejection(String);

pub struct Kernel {
    world: World,
    systems: Vec<Box<dyn System>>,
    fault: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotEnvelope {
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

    pub fn register_system(&mut self, system: Box<dyn System>) -> Result<(), KernelError> {
        let id = system.id().to_owned();
        validate_type_id(&id).map_err(|_| KernelError::InvalidSystemId(id.clone()))?;
        if self.systems.iter().any(|registered| registered.id() == id) {
            return Err(KernelError::DuplicateSystemId(id));
        }
        self.systems.push(system);
        self.systems
            .sort_by(|left, right| left.id().cmp(right.id()));
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
                rejected.push(reject(&envelope, "duplicate command ID"));
                continue;
            }
            if envelope.id.tick != current_tick {
                rejected.push(reject(&envelope, "command targets the wrong Tick"));
                continue;
            }
            if validate_type_id(&envelope.id.source).is_err() {
                rejected.push(reject(&envelope, "invalid command source ID"));
                continue;
            }
            if last_sequence_by_source
                .get(&envelope.id.source)
                .is_some_and(|last| envelope.id.sequence <= *last)
            {
                rejected.push(reject(&envelope, "sequence is not strictly increasing"));
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
                Err(reason) => rejected.push(reject(&envelope, &reason.0)),
            }
        }

        // All systems read the same world after external commands. Their outputs are
        // applied later in stable SystemId order, never through mutable World access.
        let mut system_outputs = Vec::with_capacity(self.systems.len());
        for system in &self.systems {
            match system.execute(&staged, current_tick) {
                Ok(commands) => system_outputs.push((system.id().to_owned(), commands)),
                Err(error) => {
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
                            reason: reason.to_string(),
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
            format_version: SNAPSHOT_FORMAT_VERSION,
            world: self.world.clone(),
            state_hash: state_hash(&self.world)?,
        };
        serde_json::to_vec(&envelope).map_err(|error| KernelError::Serialization(error.to_string()))
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, KernelError> {
        let envelope: SnapshotEnvelope = serde_json::from_slice(bytes)
            .map_err(|error| KernelError::Serialization(error.to_string()))?;
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

fn reject(envelope: &CommandEnvelope, reason: &str) -> CommandRejection {
    CommandRejection {
        id: envelope.id.clone(),
        reason: reason.to_owned(),
    }
}

fn apply_command(world: &mut World, command: &Command) -> Result<Option<EventKind>, Rejection> {
    match command {
        Command::SetResource { key, value } => {
            validate_type_id(key).map_err(Rejection)?;
            world.resources.insert(key.clone(), value.clone());
            Ok(Some(EventKind::ResourceSet {
                key: key.clone(),
                value: value.clone(),
            }))
        }
        Command::SpawnEntity { components } => {
            for type_id in components.keys() {
                validate_type_id(type_id).map_err(Rejection)?;
            }
            let index = world.next_entity_index;
            let next = index
                .checked_add(1)
                .ok_or_else(|| Rejection("entity ID space exhausted".to_owned()))?;
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
            world
                .entities
                .remove(entity)
                .ok_or_else(|| Rejection("target entity does not exist".to_owned()))?;
            Ok(Some(EventKind::EntityDespawned { entity: *entity }))
        }
        Command::SetComponent {
            entity,
            type_id,
            value,
        } => {
            validate_type_id(type_id).map_err(Rejection)?;
            validate_entity_target(world, *entity)?;
            let target = world
                .entities
                .get_mut(entity)
                .ok_or_else(|| Rejection("target entity does not exist".to_owned()))?;
            target.components.insert(type_id.clone(), value.clone());
            Ok(Some(EventKind::ComponentSet {
                entity: *entity,
                type_id: type_id.clone(),
            }))
        }
        Command::RemoveComponent { entity, type_id } => {
            validate_type_id(type_id).map_err(Rejection)?;
            validate_entity_target(world, *entity)?;
            let target = world
                .entities
                .get_mut(entity)
                .ok_or_else(|| Rejection("target entity does not exist".to_owned()))?;
            if target.components.remove(type_id).is_none() {
                return Err(Rejection("component does not exist".to_owned()));
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
        return Err(Rejection("entity belongs to another world".to_owned()));
    }
    if entity.generation != 0 {
        return Err(Rejection("unknown entity generation".to_owned()));
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
    let bytes =
        serde_json::to_vec(world).map_err(|error| KernelError::Serialization(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(STATE_HASH_DOMAIN);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut result = [0_u8; 32];
    result.copy_from_slice(&digest);
    Ok(StateHash(result))
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
        assert_eq!(output.committed_commands.len(), 1);
        assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(3));
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
    fn snapshot_restore_preserves_state_and_hash() {
        let mut kernel = seeded_kernel(7);
        let first = kernel.step(StepInput::default()).unwrap();
        let snapshot = kernel.snapshot().unwrap();
        let restored = Kernel::from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.world().tick(), Tick(1));
        assert_eq!(restored.world().resource_i64("org.game/counter"), Some(7));
        assert_eq!(state_hash(restored.world()).unwrap(), first.state_hash);
    }

    struct FailingSystem;

    impl System for FailingSystem {
        fn id(&self) -> &str {
            "org.game/failing"
        }

        fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
            Err(SystemError {
                system: self.id().to_owned(),
                message: "fixture failure".to_owned(),
            })
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

        assert!(matches!(result, Err(KernelError::SystemFailed(_))));
        assert_eq!(kernel.world().tick(), Tick(0));
        assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(1));
        assert!(matches!(
            kernel.step(StepInput::default()),
            Err(KernelError::Faulted(_))
        ));
    }
}
