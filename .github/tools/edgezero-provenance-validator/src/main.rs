use edgezero_provenance_validator::{ExecutionLayout, execute};

fn main() {
    if let Err(error) = execute(&ExecutionLayout::container(), std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
