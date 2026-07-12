//! A generic wrapper with a `#[app_config(nested)]` child must compile: the
//! emitted `AppConfigRoot` bound check lives in `secret_fields()` (method
//! scope) where the generic `T` resolves — at module scope it failed with
//! "cannot find type `T` in this scope".

use edgezero_core::app_config::{AppConfigMeta, AppConfigRoot};

#[derive(edgezero_core::AppConfig)]
struct Child {
    #[secret]
    token: String,
}

#[derive(edgezero_core::AppConfig)]
struct Wrapper<T: AppConfigRoot + AppConfigMeta> {
    #[app_config(nested)]
    child: T,
}

fn main() {
    let fields = Wrapper::<Child>::secret_fields();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].dotted_path(), "child.token");
}
