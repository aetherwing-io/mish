//! Policy engine for mish.
//!
//! Evaluates commands and prompts against configured policy rules
//! (forbidden, yield_to_operator, auto_confirm) to determine
//! the appropriate action.

pub mod config;
pub mod matcher;
pub mod scope;
