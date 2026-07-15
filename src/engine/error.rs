use crate::engine::agent::AgentError;
use crate::engine::parse::ParseError;
use crate::engine::prompt::ValidationError;
use crate::engine::target::TargetError;

/// Each layer keeps its own typed error; this umbrella composes them so
/// callers can still match on the originating layer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Target(#[from] TargetError),
}
