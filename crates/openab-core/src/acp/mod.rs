#[cfg(feature = "agentcore")]
pub mod agentcore;
pub mod connection;
pub mod pool;
pub mod protocol;

pub use connection::{AcpRequestError, ContentBlock};
pub use pool::{SessionPool, SessionSnapshot, SessionState};
pub use protocol::{classify_notification, parse_turn_result, AcpEvent, TurnResult};
