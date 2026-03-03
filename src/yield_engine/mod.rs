//! Yield detection engine.
//!
//! Monitors process output for signs that a process is waiting for interactive
//! input (e.g., password prompts, confirmation prompts). Uses a silence + prompt
//! heuristic and routes detected yields through the policy engine.

pub mod detector;

pub use detector::{YieldConfig, YieldDetection, YieldDetector};
