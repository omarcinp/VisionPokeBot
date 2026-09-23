use serde::{Deserialize, Serialize};

/// Where a state value came from. Strategic code prefers these categories
/// over opaque confidence scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeSource {
    Observed,
    Derived,
    Tracked,
    Assumed,
    UserProvided,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knowledge<T> {
    pub value: Option<T>,
    pub source: KnowledgeSource,
    pub last_verified_frame: Option<u64>,
}

impl<T> Knowledge<T> {
    pub fn unknown() -> Self {
        Self {
            value: None,
            source: KnowledgeSource::Unknown,
            last_verified_frame: None,
        }
    }

    pub fn observed(value: T, frame_id: u64) -> Self {
        Self {
            value: Some(value),
            source: KnowledgeSource::Observed,
            last_verified_frame: Some(frame_id),
        }
    }

    /// Worked out from static game data or story knowledge (e.g. a move's
    /// maximum PP when it is learned).
    pub fn derived(value: T, frame_id: u64) -> Self {
        Self {
            value: Some(value),
            source: KnowledgeSource::Derived,
            last_verified_frame: Some(frame_id),
        }
    }

    /// Changed by a tracked event since it was last observed at
    /// `last_verified_frame`.
    pub fn tracked(value: T, last_verified_frame: Option<u64>) -> Self {
        Self {
            value: Some(value),
            source: KnowledgeSource::Tracked,
            last_verified_frame,
        }
    }

    /// Changed by tracking (or assumed) since the last observation.
    pub fn is_stale(&self) -> bool {
        matches!(
            self.source,
            KnowledgeSource::Tracked | KnowledgeSource::Assumed
        )
    }

    /// A decision relying on this should audit it first.
    pub fn needs_audit(&self) -> bool {
        self.value.is_none() || self.is_stale()
    }
}

impl<T> Default for Knowledge<T> {
    fn default() -> Self {
        Self::unknown()
    }
}
