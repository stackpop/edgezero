pub mod archive;
pub mod elf;
pub mod extract;
pub mod json_contract;

pub type Result<T> = std::result::Result<T, String>;
