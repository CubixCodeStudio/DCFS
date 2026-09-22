//! File version states and invariants.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error when transitioning version state invalidly.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum VersionStateError {
    #[error("cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: VersionState,
        to: VersionState,
    },
}

/// The state of a file version in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionState {
    /// Version is being prepared (chunks being uploaded).
    Staging,
    /// Version is committed and visible.
    Committed,
    /// Version has been superseded by a newer version.
    Superseded,
    /// Version is marked for garbage collection.
    Garbage,
}

impl VersionState {
    /// Check if a state transition is valid.
    ///
    /// Valid transitions:
    /// - Staging -> Committed (successful commit)
    /// - Staging -> Garbage (failed upload, cleanup)
    /// - Committed -> Superseded (newer version committed)
    /// - Superseded -> Garbage (after retention period)
    pub fn can_transition_to(&self, to: VersionState) -> bool {
        matches!(
            (self, to),
            (VersionState::Staging, VersionState::Committed)
                | (VersionState::Staging, VersionState::Garbage)
                | (VersionState::Committed, VersionState::Superseded)
                | (VersionState::Superseded, VersionState::Garbage)
        )
    }

    /// Transition to a new state, returning an error if invalid.
    pub fn transition_to(self, to: VersionState) -> Result<VersionState, VersionStateError> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(VersionStateError::InvalidTransition { from: self, to })
        }
    }

    /// Check if this version is visible to readers.
    pub fn is_visible(&self) -> bool {
        matches!(self, VersionState::Committed)
    }

    /// Check if this version is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, VersionState::Garbage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_can_commit() {
        assert!(VersionState::Staging.can_transition_to(VersionState::Committed));
    }

    #[test]
    fn staging_can_become_garbage() {
        assert!(VersionState::Staging.can_transition_to(VersionState::Garbage));
    }

    #[test]
    fn committed_can_supersede() {
        assert!(VersionState::Committed.can_transition_to(VersionState::Superseded));
    }

    #[test]
    fn superseded_can_become_garbage() {
        assert!(VersionState::Superseded.can_transition_to(VersionState::Garbage));
    }

    #[test]
    fn cannot_skip_states() {
        assert!(!VersionState::Staging.can_transition_to(VersionState::Superseded));
        assert!(!VersionState::Committed.can_transition_to(VersionState::Staging));
        assert!(!VersionState::Garbage.can_transition_to(VersionState::Committed));
    }

    #[test]
    fn garbage_is_terminal() {
        assert!(VersionState::Garbage.is_terminal());
        assert!(!VersionState::Committed.is_terminal());
        assert!(!VersionState::Staging.is_terminal());
    }

    #[test]
    fn only_committed_is_visible() {
        assert!(VersionState::Committed.is_visible());
        assert!(!VersionState::Staging.is_visible());
        assert!(!VersionState::Superseded.is_visible());
        assert!(!VersionState::Garbage.is_visible());
    }

    #[test]
    fn transition_returns_error_on_invalid() {
        let result = VersionState::Garbage.transition_to(VersionState::Committed);
        assert!(result.is_err());
    }
}
