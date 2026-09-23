//! Frame normalization and file-backed video sources.

pub mod normalize;
pub mod png;
pub mod sequence;

pub use normalize::{detect_viewport, Normalizer, Rect, ViewportLocator};
pub use sequence::ImageSequenceSource;
