fn main() -> Result<(), fastly::Error> {
    edgezero_adapter_fastly::serve_app::<fixture_core::qualification::QualificationApp>(
        edgezero_adapter_fastly::Serve::new(),
    )
    .into_result()
}
