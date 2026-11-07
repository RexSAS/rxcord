//! QR-code login via Discord's remote-auth gateway.
mod driver;
pub mod protocol;
pub use driver::{QrStatus, qr_login};
pub use protocol::{RaAction, RaError, RaProtocol};
