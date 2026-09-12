pub mod archive;
mod command;
pub mod elf;
pub mod extract;
pub mod json_contract;
mod orchestration;
mod self_test;

pub use orchestration::{ExecutionLayout, execute};

pub type Result<T> = std::result::Result<T, String>;

pub(crate) fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}
