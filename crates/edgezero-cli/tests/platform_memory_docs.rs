#![cfg(all(
    feature = "edgezero-adapter-axum",
    feature = "edgezero-adapter-cloudflare",
    feature = "edgezero-adapter-fastly",
    feature = "edgezero-adapter-spin"
))]

#[cfg(test)]
mod tests {
    use edgezero_core::{MemoryCeilingScope, MemoryCeilingSource, PlatformMetadata};

    const CAPABILITY_GUIDE: &str = include_str!("../../../docs/guide/capabilities.md");

    fn normalized_row(row: &str) -> String {
        row.trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("|")
    }

    fn guide_contains_row(expected: &str) -> bool {
        let normalized_expected = normalized_row(expected);
        CAPABILITY_GUIDE
            .lines()
            .any(|line| normalized_row(line) == normalized_expected)
    }

    fn scope_label(scope: MemoryCeilingScope) -> &'static str {
        match scope {
            MemoryCeilingScope::PerExecution => "Per execution",
            MemoryCeilingScope::PerInstance => "Per instance",
            _ => "Unknown",
        }
    }

    fn source_label(source: MemoryCeilingSource) -> String {
        match source {
            MemoryCeilingSource::PlatformLimit { provider } => {
                format!("Platform limit: {provider}")
            }
            MemoryCeilingSource::HostedDefault { provider } => {
                format!("Hosted default: {provider}")
            }
            MemoryCeilingSource::RuntimeConfiguration { runtime } => {
                format!("Runtime configuration: {runtime}")
            }
            _ => "Unknown".to_owned(),
        }
    }

    fn assert_documented_platform(
        target: &str,
        platform: PlatformMetadata,
        primary_unit: &str,
        stack_unit: Option<&str>,
    ) {
        let ceiling = platform
            .memory_ceiling()
            .expect("known platform must publish a ceiling");
        let primary = format!("{} bytes ({primary_unit})", ceiling.total_bytes());
        let stack = ceiling.stack_bytes().map_or_else(
            || "None".to_owned(),
            |bytes| {
                format!(
                    "{} bytes ({})",
                    bytes,
                    stack_unit.expect("documented stack unit")
                )
            },
        );
        let row = format!(
            "| {target} | {primary} | {stack} | {} | {} |",
            scope_label(ceiling.scope()),
            source_label(ceiling.source())
        );
        assert!(
            guide_contains_row(&row),
            "capability guide is missing canonical row: {row}"
        );
    }

    #[test]
    fn platform_memory_table_matches_adapter_constants() {
        assert!(
            edgezero_adapter_axum::AXUM_PLATFORM
                .memory_ceiling()
                .is_none()
        );
        assert!(guide_contains_row(
            "| Axum | Unknown | None | Operator-defined | Operator configuration |"
        ));

        assert_documented_platform(
            "Cloudflare Workers",
            edgezero_adapter_cloudflare::CLOUDFLARE_PLATFORM,
            "128 MB",
            None,
        );
        assert_documented_platform(
            "Fastly Compute",
            edgezero_adapter_fastly::FASTLY_PLATFORM,
            "128 MB",
            Some("1 MB"),
        );

        assert!(
            edgezero_adapter_spin::SPIN_PLATFORM
                .memory_ceiling()
                .is_none()
        );
        assert!(guide_contains_row(
            "| Spin (generic) | Unknown | None | Runtime-configured | Spin runtime |"
        ));
        assert_documented_platform(
            "Akamai Functions (Spin)",
            edgezero_adapter_spin::AKAMAI_FUNCTIONS_PLATFORM,
            "128 MiB",
            None,
        );
    }
}
