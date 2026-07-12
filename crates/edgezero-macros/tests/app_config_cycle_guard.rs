//! Runtime backstop: a MUTUAL `#[app_config(nested)]` cycle — which the derive
//! cannot detect at compile time (it sees one type at a time) — panics with a
//! clear message on first `secret_fields()` call instead of overflowing the
//! stack (an undiagnosable trap on WASM).

#[cfg(test)]
mod tests {
    use edgezero_core::app_config::{AppConfigMeta as _, SecretField};

    // `Alpha` nests `Vec<Beta>` and `Beta` nests `Vec<Alpha>`: both compile (the
    // child ident is never the enclosing struct's), but `secret_fields()`
    // recurses forever.
    #[derive(edgezero_core::AppConfig)]
    struct Alpha {
        #[app_config(nested)]
        #[expect(
            dead_code,
            reason = "fixture field; only its type drives secret_fields()"
        )]
        betas: Vec<Beta>,
    }

    #[derive(edgezero_core::AppConfig)]
    struct Beta {
        #[app_config(nested)]
        #[expect(
            dead_code,
            reason = "fixture field; only its type drives secret_fields()"
        )]
        alphas: Vec<Alpha>,
    }

    #[test]
    #[should_panic(expected = "cyclic nesting")]
    fn mutual_nested_cycle_panics_instead_of_overflowing() {
        let _fields: Vec<SecretField> = Alpha::secret_fields();
    }
}
