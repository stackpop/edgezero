#![cfg(all(
    feature = "edgezero-adapter-axum",
    feature = "edgezero-adapter-cloudflare",
    feature = "edgezero-adapter-fastly",
    feature = "edgezero-adapter-spin"
))]

#[cfg(test)]
mod tests {
    use edgezero_core::{
        HostIngressMemoryAccounting, InboundRequestPopulationBound, MemoryCeiling,
        MemoryCeilingScope, PlatformFact, PlatformMetadata, PlatformResourceSource,
        PlatformUnknownReason,
    };

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
            _ => panic!("unrecognized memory ceiling scope"),
        }
    }

    fn source_label(source: PlatformResourceSource) -> String {
        match source {
            PlatformResourceSource::PlatformLimit { provider } => {
                format!("Platform limit: {provider}")
            }
            PlatformResourceSource::HostedDefault { provider } => {
                format!("Hosted default: {provider}")
            }
            PlatformResourceSource::RuntimeConfiguration { runtime } => {
                format!("Runtime configuration: {runtime}")
            }
            _ => panic!("unrecognized platform resource source"),
        }
    }

    fn unknown_reason_label(reason: PlatformUnknownReason) -> &'static str {
        match reason {
            PlatformUnknownReason::OperatorConfigured => "Unknown: Operator configured",
            PlatformUnknownReason::ProviderUnpublished => "Unknown: Provider unpublished",
            PlatformUnknownReason::RuntimeConfigured => "Unknown: Runtime configured",
            PlatformUnknownReason::Unspecified => "Unknown: Unspecified",
            _ => panic!("unrecognized platform unknown reason"),
        }
    }

    fn byte_unit_label(bytes: u64) -> String {
        const BYTES_PER_MB: u64 = 1_000_000;
        const BYTES_PER_MIB: u64 = 1024 * 1024;

        if let Some(megabytes) = bytes
            .checked_div(BYTES_PER_MB)
            .filter(|_| bytes.is_multiple_of(BYTES_PER_MB))
        {
            format!("{megabytes} MB")
        } else if let Some(mebibytes) = bytes
            .checked_div(BYTES_PER_MIB)
            .filter(|_| bytes.is_multiple_of(BYTES_PER_MIB))
        {
            format!("{mebibytes} MiB")
        } else {
            format!("{bytes} B")
        }
    }

    fn bytes_with_unit(bytes: u64) -> String {
        format!("{bytes} bytes ({})", byte_unit_label(bytes))
    }

    #[test]
    fn byte_units_are_derived_from_canonical_values() {
        let bytes_134_217_728 = 128 * 1024 * 1024;

        assert_eq!(byte_unit_label(128_000_000), "128 MB");
        assert_eq!(byte_unit_label(1_000_000), "1 MB");
        assert_eq!(byte_unit_label(bytes_134_217_728), "128 MiB");
        assert_eq!(byte_unit_label(1_234), "1234 B");
    }

    fn memory_cells(fact: PlatformFact<MemoryCeiling>) -> (String, String, String, String) {
        match fact {
            PlatformFact::Known { value, source } => {
                let primary = bytes_with_unit(value.total_bytes());
                let stack = value
                    .stack_bytes()
                    .map_or_else(|| "None".to_owned(), bytes_with_unit);
                (
                    primary,
                    stack,
                    scope_label(value.scope()).to_owned(),
                    source_label(source),
                )
            }
            PlatformFact::Unknown { reason } => (
                "Unknown".to_owned(),
                "Unknown".to_owned(),
                "Unknown".to_owned(),
                unknown_reason_label(reason).to_owned(),
            ),
            _ => panic!("unrecognized memory fact variant"),
        }
    }

    fn population_cells(fact: PlatformFact<InboundRequestPopulationBound>) -> (String, String) {
        match fact {
            PlatformFact::Known { value, source } => {
                (value.max_live_requests().to_string(), source_label(source))
            }
            PlatformFact::Unknown { reason } => (
                "Unknown".to_owned(),
                unknown_reason_label(reason).to_owned(),
            ),
            _ => panic!("unrecognized population fact variant"),
        }
    }

    fn host_accounting_cells(fact: PlatformFact<HostIngressMemoryAccounting>) -> (String, String) {
        match fact {
            PlatformFact::Known { value, source } => {
                let accounting = match value {
                    HostIngressMemoryAccounting::CountsTowardCeiling => "Counts toward ceiling",
                    HostIngressMemoryAccounting::OutsideCeiling => "Outside ceiling",
                    _ => panic!("unrecognized host ingress memory accounting"),
                };
                (accounting.to_owned(), source_label(source))
            }
            PlatformFact::Unknown { reason } => (
                "Unknown".to_owned(),
                unknown_reason_label(reason).to_owned(),
            ),
            _ => panic!("unrecognized host accounting fact variant"),
        }
    }

    fn assert_documented_platform(target: &str, platform: PlatformMetadata) {
        let (primary, stack, scope, memory_source) = memory_cells(platform.memory_ceiling());
        let (population, population_source) =
            population_cells(platform.inbound_request_population_bound());
        let (host_accounting, host_accounting_source) =
            host_accounting_cells(platform.host_ingress_memory_accounting());
        let row = format!(
            "| {target} | {primary} | {stack} | {scope} | {memory_source} | {population} | {population_source} | {host_accounting} | {host_accounting_source} |"
        );
        assert!(
            guide_contains_row(&row),
            "capability guide is missing canonical row: {row}"
        );
    }

    #[test]
    fn platform_resource_table_matches_adapter_constants() {
        assert_documented_platform("Axum", edgezero_adapter_axum::AXUM_PLATFORM);
        assert_documented_platform(
            "Cloudflare Workers",
            edgezero_adapter_cloudflare::CLOUDFLARE_PLATFORM,
        );
        assert_documented_platform("Fastly Compute", edgezero_adapter_fastly::FASTLY_PLATFORM);
        assert_documented_platform("Spin (generic)", edgezero_adapter_spin::SPIN_PLATFORM);
        assert_documented_platform(
            "Akamai Functions (Spin)",
            edgezero_adapter_spin::AKAMAI_FUNCTIONS_PLATFORM,
        );
    }
}
