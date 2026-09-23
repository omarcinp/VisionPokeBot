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
}

impl<T> Default for Knowledge<T> {
    fn default() -> Self {
        Self::unknown()
    }
}
