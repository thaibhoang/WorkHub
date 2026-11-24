use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::ids::{FactoryId, SessionId};

/// Who caused an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    Human { user: String },
    Agent { session: SessionId },
    System { component: String },
}

/// Metadata carried by every event. `correlation_id` groups everything belonging
/// to one task; `causation_id` points at the event that triggered this one, so a
/// full cause/effect tree (and timeline) can be rebuilt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMeta {
    pub occurred_at: DateTime<Utc>,
    pub correlation_id: Uuid,
    pub causation_id: Option<Uuid>,
    pub actor: Actor,
    /// Optional until factory-as-code exists (phase 2).
    pub factory_id: Option<FactoryId>,
    /// Git sha of the factory definition in force; enables before/after comparison.
    pub factory_version: Option<String>,
}

impl EventMeta {
    pub fn new(correlation_id: Uuid, actor: Actor) -> Self {
        Self {
            occurred_at: Utc::now(),
            correlation_id,
            causation_id: None,
            actor,
            factory_id: None,
            factory_version: None,
        }
    }

    pub fn caused_by(mut self, event_id: Uuid) -> Self {
        self.causation_id = Some(event_id);
        self
    }
}

/// A domain event enum (e.g. `TaskEvent`). The serialized form must be
/// internally tagged so the payload is self-describing.
pub trait DomainEvent: Serialize + DeserializeOwned {
    const SCHEMA_VERSION: i16 = 1;
    fn event_type(&self) -> &'static str;
}

/// An event ready to be appended (no sequence numbers yet).
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub schema_version: i16,
    pub payload: Value,
    pub meta: EventMeta,
}

impl NewEvent {
    pub fn from_domain<E: DomainEvent>(event: &E, meta: EventMeta) -> Result<Self, serde_json::Error> {
        Ok(Self {
            event_id: Uuid::now_v7(),
            event_type: event.event_type().to_owned(),
            schema_version: E::SCHEMA_VERSION,
            payload: serde_json::to_value(event)?,
            meta,
        })
    }
}

/// An event as stored in the log.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub global_seq: i64,
    pub event_id: Uuid,
    pub stream_id: String,
    pub stream_version: i64,
    pub event_type: String,
    pub schema_version: i16,
    pub payload: Value,
    pub meta: EventMeta,
    pub recorded_at: DateTime<Utc>,
}

impl RecordedEvent {
    pub fn decode<E: DomainEvent>(&self) -> Result<E, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}
