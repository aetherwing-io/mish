use serde::Serialize;

/// Process lifecycle states.
///
/// Every spawned process moves through these states according to defined
/// transitions. Terminal states have no outgoing edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Completed,
    Failed,
    AwaitingInput,
    /// Operator handoff — defined now for Phase 3.
    HandedOff,
    Killed,
    TimedOut,
}

impl ProcessState {
    /// State name as lowercase underscore-separated string.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcessState::Running => "running",
            ProcessState::Completed => "completed",
            ProcessState::Failed => "failed",
            ProcessState::AwaitingInput => "awaiting_input",
            ProcessState::HandedOff => "handed_off",
            ProcessState::Killed => "killed",
            ProcessState::TimedOut => "timed_out",
        }
    }

    /// Check if a transition from `self` to `target` is valid.
    ///
    /// Valid transitions:
    /// - running -> completed, failed, awaiting_input, killed, timed_out, handed_off
    /// - awaiting_input -> running, handed_off, killed
    /// - handed_off -> running, completed, failed
    pub fn can_transition_to(&self, target: ProcessState) -> bool {
        matches!(
            (self, target),
            (ProcessState::Running, ProcessState::Completed)
                | (ProcessState::Running, ProcessState::Failed)
                | (ProcessState::Running, ProcessState::AwaitingInput)
                | (ProcessState::Running, ProcessState::Killed)
                | (ProcessState::Running, ProcessState::TimedOut)
                | (ProcessState::Running, ProcessState::HandedOff)
                | (ProcessState::AwaitingInput, ProcessState::Running)
                | (ProcessState::AwaitingInput, ProcessState::HandedOff)
                | (ProcessState::AwaitingInput, ProcessState::Killed)
                | (ProcessState::HandedOff, ProcessState::Running)
                | (ProcessState::HandedOff, ProcessState::Completed)
                | (ProcessState::HandedOff, ProcessState::Failed)
        )
    }

    /// Check if this is a terminal state (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ProcessState::Completed
                | ProcessState::Failed
                | ProcessState::Killed
                | ProcessState::TimedOut
        )
    }
}

impl std::fmt::Display for ProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Valid transitions ──────────────────────────────────────────

    #[test]
    fn running_to_completed() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::Completed));
    }

    #[test]
    fn running_to_failed() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::Failed));
    }

    #[test]
    fn running_to_awaiting_input() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::AwaitingInput));
    }

    #[test]
    fn running_to_killed() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::Killed));
    }

    #[test]
    fn running_to_timed_out() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::TimedOut));
    }

    #[test]
    fn running_to_handed_off() {
        assert!(ProcessState::Running.can_transition_to(ProcessState::HandedOff));
    }

    #[test]
    fn awaiting_input_to_running() {
        assert!(ProcessState::AwaitingInput.can_transition_to(ProcessState::Running));
    }

    #[test]
    fn awaiting_input_to_handed_off() {
        assert!(ProcessState::AwaitingInput.can_transition_to(ProcessState::HandedOff));
    }

    #[test]
    fn awaiting_input_to_killed() {
        assert!(ProcessState::AwaitingInput.can_transition_to(ProcessState::Killed));
    }

    #[test]
    fn handed_off_to_running() {
        assert!(ProcessState::HandedOff.can_transition_to(ProcessState::Running));
    }

    #[test]
    fn handed_off_to_completed() {
        assert!(ProcessState::HandedOff.can_transition_to(ProcessState::Completed));
    }

    #[test]
    fn handed_off_to_failed() {
        assert!(ProcessState::HandedOff.can_transition_to(ProcessState::Failed));
    }

    // ── Invalid transitions ────────────────────────────────────────

    #[test]
    fn completed_to_running_invalid() {
        assert!(!ProcessState::Completed.can_transition_to(ProcessState::Running));
    }

    #[test]
    fn failed_to_running_invalid() {
        assert!(!ProcessState::Failed.can_transition_to(ProcessState::Running));
    }

    #[test]
    fn killed_to_any_invalid() {
        for target in all_states() {
            assert!(
                !ProcessState::Killed.can_transition_to(target),
                "killed -> {} should be invalid",
                target.as_str()
            );
        }
    }

    #[test]
    fn timed_out_to_any_invalid() {
        for target in all_states() {
            assert!(
                !ProcessState::TimedOut.can_transition_to(target),
                "timed_out -> {} should be invalid",
                target.as_str()
            );
        }
    }

    #[test]
    fn completed_to_any_invalid() {
        for target in all_states() {
            assert!(
                !ProcessState::Completed.can_transition_to(target),
                "completed -> {} should be invalid",
                target.as_str()
            );
        }
    }

    #[test]
    fn failed_to_any_invalid() {
        for target in all_states() {
            assert!(
                !ProcessState::Failed.can_transition_to(target),
                "failed -> {} should be invalid",
                target.as_str()
            );
        }
    }

    #[test]
    fn self_transitions_invalid() {
        for state in all_states() {
            assert!(
                !state.can_transition_to(state),
                "{} -> {} (self) should be invalid",
                state.as_str(),
                state.as_str()
            );
        }
    }

    // ── is_terminal ────────────────────────────────────────────────

    #[test]
    fn terminal_states() {
        assert!(ProcessState::Completed.is_terminal());
        assert!(ProcessState::Failed.is_terminal());
        assert!(ProcessState::Killed.is_terminal());
        assert!(ProcessState::TimedOut.is_terminal());
    }

    #[test]
    fn non_terminal_states() {
        assert!(!ProcessState::Running.is_terminal());
        assert!(!ProcessState::AwaitingInput.is_terminal());
        assert!(!ProcessState::HandedOff.is_terminal());
    }

    // ── as_str ─────────────────────────────────────────────────────

    #[test]
    fn as_str_values() {
        assert_eq!(ProcessState::Running.as_str(), "running");
        assert_eq!(ProcessState::Completed.as_str(), "completed");
        assert_eq!(ProcessState::Failed.as_str(), "failed");
        assert_eq!(ProcessState::AwaitingInput.as_str(), "awaiting_input");
        assert_eq!(ProcessState::HandedOff.as_str(), "handed_off");
        assert_eq!(ProcessState::Killed.as_str(), "killed");
        assert_eq!(ProcessState::TimedOut.as_str(), "timed_out");
    }

    // ── Serialize ──────────────────────────────────────────────────

    #[test]
    fn serialize_produces_correct_json() {
        let json = serde_json::to_string(&ProcessState::AwaitingInput).unwrap();
        assert_eq!(json, r#""awaiting_input""#);

        let json = serde_json::to_string(&ProcessState::HandedOff).unwrap();
        assert_eq!(json, r#""handed_off""#);

        let json = serde_json::to_string(&ProcessState::Running).unwrap();
        assert_eq!(json, r#""running""#);

        let json = serde_json::to_string(&ProcessState::TimedOut).unwrap();
        assert_eq!(json, r#""timed_out""#);
    }

    // ── helpers ────────────────────────────────────────────────────

    fn all_states() -> Vec<ProcessState> {
        vec![
            ProcessState::Running,
            ProcessState::Completed,
            ProcessState::Failed,
            ProcessState::AwaitingInput,
            ProcessState::HandedOff,
            ProcessState::Killed,
            ProcessState::TimedOut,
        ]
    }
}
