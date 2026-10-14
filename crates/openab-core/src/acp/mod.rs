#[cfg(feature = "agentcore")]
pub mod agentcore;
pub mod config_prefs;
pub mod connection;
pub mod pool;
pub mod protocol;

pub use connection::{AcpRequestError, ContentBlock};
pub use pool::{ConfigChoiceOutcome, SessionPool, SessionPoolEntry, SessionSnapshot, SessionState};
pub use protocol::{classify_notification, parse_turn_result, AcpEvent, TurnResult};
