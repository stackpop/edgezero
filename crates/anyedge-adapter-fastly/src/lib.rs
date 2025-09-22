//! Utilities for bridging Fastly Compute@Edge requests into the
//! `anyedge-core` service abstractions.

#[cfg(feature = "cli")]
pub mod cli;
mod context;
#[cfg(feature = "fastly")]
mod logger;
#[cfg(feature = "fastly")]
mod proxy;
#[cfg(feature = "fastly")]
mod request;
#[cfg(feature = "fastly")]
mod response;

pub use context::FastlyRequestContext;
#[cfg(feature = "fastly")]
pub use proxy::FastlyProxyClient;
#[cfg(feature = "fastly")]
pub use request::{dispatch, into_core_request};
#[cfg(feature = "fastly")]
pub use response::from_core_response;

#[cfg(feature = "fastly")]
pub fn init_logger(
    endpoint: &str,
    level: log::LevelFilter,
    echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    logger::init_logger(endpoint, level, echo_stdout)
}

#[cfg(not(feature = "fastly"))]
pub fn init_logger(
    _endpoint: &str,
    _level: log::LevelFilter,
    _echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    Ok(())
}

#[cfg(feature = "fastly")]
pub trait AppExt {
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error>;
}

#[cfg(feature = "fastly")]
impl AppExt for anyedge_core::App {
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
        dispatch(self, req)
    }
}
