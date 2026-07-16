pub const DEFAULT_Q_TRANSPORT_REGION: &str = "us-east-1";

const KNOWN_BAD_Q_TRANSPORT_REGIONS: &[&str] = &["eu-north-1"];

pub fn trimmed_region(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub fn is_known_bad_q_transport_region(value: &str) -> bool {
    trimmed_region(value).is_some_and(|region| {
        KNOWN_BAD_Q_TRANSPORT_REGIONS
            .iter()
            .any(|known_bad| region.eq_ignore_ascii_case(known_bad))
    })
}

pub fn normalize_q_transport_region(value: &str) -> &str {
    let Some(region) = trimmed_region(value) else {
        return DEFAULT_Q_TRANSPORT_REGION;
    };
    if is_known_bad_q_transport_region(region) {
        DEFAULT_Q_TRANSPORT_REGION
    } else {
        region
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_region_trims_known_good_values() {
        assert_eq!(
            normalize_q_transport_region("  eu-central-1  "),
            "eu-central-1"
        );
    }

    #[test]
    fn transport_region_maps_known_bad_values_case_insensitively() {
        assert_eq!(
            normalize_q_transport_region(" EU-NORTH-1 "),
            DEFAULT_Q_TRANSPORT_REGION
        );
    }

    #[test]
    fn transport_region_defaults_empty_values() {
        assert_eq!(
            normalize_q_transport_region("   "),
            DEFAULT_Q_TRANSPORT_REGION
        );
    }

    #[test]
    fn transport_region_normalization_is_idempotent() {
        let once = normalize_q_transport_region(" eu-north-1 ");
        assert_eq!(normalize_q_transport_region(once), once);
    }
}
