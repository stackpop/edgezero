//! Fastly adapter for AnyEdge.
//!
//! Usage (in your Fastly Compute@Edge binary crate):
//!
//! ```ignore
//! use anyedge_fastly as aef;
//!
//! #[fastly::main]
//! fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
//!     let app = build_app();
//!     // Initialize formatted Fastly logger (single API)
//!     aef::init_logger("your_endpoint", log::LevelFilter::Info, true)?;
//!     Ok(aef::handle(&app, req))
//! }
//! ```

#[cfg(feature = "fastly")]
pub mod app;
#[cfg(feature = "fastly")]
pub mod http;
#[cfg(feature = "fastly")]
pub mod logging;
#[cfg(feature = "fastly")]
pub mod proxy;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(not(feature = "fastly"))]
mod stub;

#[cfg(feature = "fastly")]
pub use app::handle;
#[cfg(feature = "fastly")]
pub use logging::init_logger;
#[cfg(feature = "fastly")]
pub use proxy::register_proxy;
#[cfg(not(feature = "fastly"))]
pub use stub::handle;
