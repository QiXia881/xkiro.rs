use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::common::io::{atomic_write_string_secure, resolve_symlink_target};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::{DEFAULT_Q_TRANSPORT_REGION, trimmed_region};
use crate::model::config::Config;

pub struct RepairApiRegionOptions {
    pub credentials_path: PathBuf,
    pub config: Config,
    pub target_api_region: String,
    pub known_bad_api_regions: Vec<String>,
    pub check_dns: bool,
    pub apply: bool,
    pub service_stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairApiRegionChange {
    pub id: String,
    pub email: String,
    pub old_api_region: Option<String>,
    pub effective_transport_region: String,
    pub new_api_region: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct RepairApiRegionReport {
    pub credentials_path: PathBuf,
    pub changes: Vec<RepairApiRegionChange>,
    pub backup_path: Option<PathBuf>,
    pub applied: bool,
}

pub fn run(options: &RepairApiRegionOptions) -> anyhow::Result<RepairApiRegionReport> {
    run_with(
        options,
        |host| {
            (host, 443)
                .to_socket_addrs()
                .map(|mut addresses| addresses.next().is_some())
                .unwrap_or(false)
        },
        |path, content| atomic_write_string_secure(path, content),
    )
}

pub fn print_report(report: &RepairApiRegionReport) {
    if report.changes.is_empty() {
        println!("No credential needs apiRegion repair.");
        return;
    }

    for change in &report.changes {
        println!(
            "id={} email={} oldApiRegion={} effectiveTransport={} newApiRegion={} reason={}",
            change.id,
            change.email,
            change.old_api_region.as_deref().unwrap_or("(empty)"),
            change.effective_transport_region,
            change.new_api_region,
            change.reason
        );
    }

    if report.applied {
        println!("Updated {}", report.credentials_path.display());
        if let Some(path) = &report.backup_path {
            println!("Backup: {}", path.display());
        }
    } else {
        println!("Dry run only. Rerun with --apply --service-stopped to write changes.");
    }
}

fn run_with<D, W>(
    options: &RepairApiRegionOptions,
    dns_resolves: D,
    writer: W,
) -> anyhow::Result<RepairApiRegionReport>
where
    D: Fn(&str) -> bool,
    W: Fn(&Path, &str) -> std::io::Result<()>,
{
    let target = trimmed_region(&options.target_api_region)
        .context("target api region must not be empty")?;
    let known_bad: Vec<&str> = options
        .known_bad_api_regions
        .iter()
        .filter_map(|region| trimmed_region(region))
        .collect();
    if known_bad.is_empty() {
        bail!("known bad api region list must not be empty");
    }
    if region_is_known_bad(target, &known_bad) {
        bail!("target api region must not also be known bad: {target}");
    }
    if options.apply && !options.service_stopped {
        bail!("--apply requires --service-stopped after stopping the xkiro service");
    }

    let credentials_path = resolve_symlink_target(&options.credentials_path);
    let original = std::fs::read_to_string(&credentials_path)
        .with_context(|| format!("read credentials file {}", credentials_path.display()))?;
    if original.trim().is_empty() {
        bail!("credentials file is empty: {}", credentials_path.display());
    }
    let mut root: Value = serde_json::from_str(&original)
        .with_context(|| format!("parse credentials file {}", credentials_path.display()))?;

    let mut changes = Vec::new();
    match &mut root {
        Value::Object(object) => {
            if object.contains_key("accounts") || object.contains_key("credentials") {
                bail!(
                    "merged config objects are not supported; pass the xkiro credentials.json file"
                );
            }
            repair_object(
                object,
                &options.config,
                target,
                &known_bad,
                options.check_dns,
                &dns_resolves,
                &mut changes,
            )?;
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let object = item
                    .as_object_mut()
                    .with_context(|| format!("credential array item {index} must be an object"))?;
                repair_object(
                    object,
                    &options.config,
                    target,
                    &known_bad,
                    options.check_dns,
                    &dns_resolves,
                    &mut changes,
                )?;
            }
        }
        _ => bail!("credentials file must contain one object or an array of objects"),
    }

    let mut backup_path = None;
    if options.apply && !changes.is_empty() {
        let backup = make_backup_path(&credentials_path);
        std::fs::copy(&credentials_path, &backup).with_context(|| {
            format!(
                "create credentials backup {} from {}",
                backup.display(),
                credentials_path.display()
            )
        })?;
        let mut updated = serde_json::to_string_pretty(&root)?;
        updated.push('\n');
        if let Err(write_error) = writer(&credentials_path, &updated) {
            if let Err(restore_error) = std::fs::copy(&backup, &credentials_path) {
                bail!(
                    "write repaired credentials {} failed: {}; restore from {} failed: {}; backup remains available",
                    credentials_path.display(),
                    write_error,
                    backup.display(),
                    restore_error
                );
            }
            bail!(
                "write repaired credentials {} failed: {}; original restored from {}; backup remains available",
                credentials_path.display(),
                write_error,
                backup.display()
            );
        }
        backup_path = Some(backup);
    }

    Ok(RepairApiRegionReport {
        credentials_path,
        changes,
        backup_path,
        applied: options.apply,
    })
}

fn repair_object<D>(
    object: &mut Map<String, Value>,
    config: &Config,
    target: &str,
    known_bad: &[&str],
    check_dns: bool,
    dns_resolves: &D,
    changes: &mut Vec<RepairApiRegionChange>,
) -> anyhow::Result<()>
where
    D: Fn(&str) -> bool,
{
    let analysis = analyze_object(object, config, target, known_bad, check_dns, dns_resolves)?;
    let Some(change) = analysis else {
        return Ok(());
    };
    object.insert(
        "apiRegion".to_string(),
        Value::String(change.new_api_region.clone()),
    );
    changes.push(change);
    Ok(())
}

fn analyze_object<D>(
    object: &Map<String, Value>,
    config: &Config,
    target: &str,
    known_bad: &[&str],
    check_dns: bool,
    dns_resolves: &D,
) -> anyhow::Result<Option<RepairApiRegionChange>>
where
    D: Fn(&str) -> bool,
{
    let api_region = string_field(object, "apiRegion")?;
    let credential_region = string_field(object, "region")?;
    let profile_arn = string_field(object, "profileArn")?;
    let endpoint = string_field(object, "endpoint")?.unwrap_or(&config.default_endpoint);
    let profile_region = profile_arn.and_then(KiroCredentials::profile_arn_region_from_value);
    let config_api_region = config.api_region.as_deref().and_then(trimmed_region);
    let config_region = trimmed_region(&config.region).unwrap_or(DEFAULT_Q_TRANSPORT_REGION);
    let configured_api_region = api_region.or(config_api_region);
    let api_transport_region = configured_api_region.unwrap_or(config_region);
    let kiro_transport_region = match profile_region {
        Some(region) if region_is_known_bad(region, known_bad) => {
            configured_api_region.unwrap_or(target)
        }
        Some(region) => region,
        None => api_transport_region,
    };
    let request_transport_region = if endpoint.eq_ignore_ascii_case("cli") {
        api_transport_region
    } else {
        kiro_transport_region
    };
    let profile_source_needs_override = profile_region
        .is_some_and(|region| region_is_known_bad(region, known_bad))
        && configured_api_region.is_none();
    let profile_lookup_region = match profile_region {
        Some(region) if region_is_known_bad(region, known_bad) => {
            configured_api_region.unwrap_or(target)
        }
        Some(region) => region,
        None => api_region
            .or(credential_region)
            .or(config_api_region)
            .unwrap_or(config_region),
    };

    let reason = if profile_source_needs_override {
        Some(format!(
            "known bad profile region '{}' has no apiRegion override",
            profile_region.unwrap_or_default()
        ))
    } else if region_is_known_bad(request_transport_region, known_bad) {
        Some(format!(
            "known bad request transport region '{request_transport_region}'"
        ))
    } else if region_is_known_bad(profile_lookup_region, known_bad) {
        Some(format!(
            "known bad profile lookup region '{profile_lookup_region}'"
        ))
    } else if check_dns && !request_transport_region.eq_ignore_ascii_case(target) {
        let host = format!("q.{request_transport_region}.amazonaws.com");
        (!dns_resolves(&host)).then(|| format!("DNS lookup failed for {host}"))
    } else {
        None
    };

    let Some(reason) = reason else {
        return Ok(None);
    };

    Ok(Some(RepairApiRegionChange {
        id: display_id(object),
        email: mask_email(string_field(object, "email")?),
        old_api_region: api_region.map(str::to_string),
        effective_transport_region: request_transport_region.to_string(),
        new_api_region: target.to_string(),
        reason,
    }))
}

fn string_field<'a>(object: &'a Map<String, Value>, name: &str) -> anyhow::Result<Option<&'a str>> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(trimmed_region(value)),
        Some(_) => bail!("credential field {name} must be a string or null"),
    }
}

fn display_id(object: &Map<String, Value>) -> String {
    object
        .get("id")
        .or_else(|| object.get("sourceAccountId"))
        .map(|value| match value {
            Value::String(value) => value.clone(),
            _ => value.to_string(),
        })
        .unwrap_or_else(|| "(no-id)".to_string())
}

fn mask_email(email: Option<&str>) -> String {
    let Some((name, domain)) = email.and_then(|value| value.split_once('@')) else {
        return "(no-email)".to_string();
    };
    if name.chars().count() <= 2 {
        format!("{}@{domain}", "*".repeat(name.chars().count()))
    } else {
        let prefix: String = name.chars().take(2).collect();
        format!("{prefix}***@{domain}")
    }
}

fn region_is_known_bad(region: &str, known_bad: &[&str]) -> bool {
    trimmed_region(region).is_some_and(|region| {
        known_bad
            .iter()
            .any(|candidate| region.eq_ignore_ascii_case(candidate))
    })
}

fn make_backup_path(path: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("credentials.json");
    path.with_file_name(format!(
        "{file_name}.bak.{stamp}.{}.{nanos:09}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "xkiro-api-region-repair-{}-{nanos}-{name}.json",
            std::process::id()
        ))
    }

    fn options(path: PathBuf) -> RepairApiRegionOptions {
        RepairApiRegionOptions {
            credentials_path: path,
            config: Config::default(),
            target_api_region: "us-east-1".to_string(),
            known_bad_api_regions: vec!["eu-north-1".to_string()],
            check_dns: false,
            apply: false,
            service_stopped: false,
        }
    }

    #[test]
    fn dry_run_reports_array_changes_without_writing() {
        let path = temp_path("dry-run");
        let original = r#"[
  {"id":1,"email":"alice@example.com","region":"eu-north-1","unknown":{"keep":true}},
  {"id":2,"apiRegion":"eu-central-1"}
]"#;
        std::fs::write(&path, original).unwrap();

        let report = run_with(&options(path.clone()), |_| true, |_, _| Ok(())).unwrap();

        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].id, "1");
        assert_eq!(report.changes[0].email, "al***@example.com");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!report.applied);
        assert!(report.backup_path.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn apply_repairs_single_object_with_backup_and_is_idempotent() {
        let path = temp_path("single");
        let original = r#"{"id":"acct-1","profileArn":"arn:aws:codewhisperer:eu-north-1:123:profile/test","unknown":42}"#;
        std::fs::write(&path, original).unwrap();
        let mut options = options(path.clone());
        options.apply = true;
        options.service_stopped = true;

        let first = run_with(
            &options,
            |_| true,
            |path, content| atomic_write_string_secure(path, content),
        )
        .unwrap();

        assert_eq!(first.changes.len(), 1);
        let backup = first.backup_path.unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["apiRegion"], "us-east-1");
        assert_eq!(updated["unknown"], 42);

        let second = run_with(
            &options,
            |_| true,
            |path, content| atomic_write_string_secure(path, content),
        )
        .unwrap();
        assert!(second.changes.is_empty());
        assert!(second.backup_path.is_none());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup);
    }

    #[test]
    fn merged_accounts_shape_is_rejected() {
        let path = temp_path("accounts");
        std::fs::write(&path, r#"{"accounts":[]}"#).unwrap();

        let error = run_with(&options(path.clone()), |_| true, |_, _| Ok(()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("merged config objects are not supported"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn valid_explicit_api_region_does_not_repair_known_bad_profile() {
        let path = temp_path("explicit");
        let original = r#"{"apiRegion":"eu-central-1","profileArn":"arn:aws:codewhisperer:eu-north-1:123:profile/test"}"#;
        std::fs::write(&path, original).unwrap();

        let report = run_with(&options(path.clone()), |_| true, |_, _| Ok(())).unwrap();

        assert!(report.changes.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn optional_dns_check_uses_injected_resolver() {
        let path = temp_path("dns");
        std::fs::write(&path, r#"{"apiRegion":"moon-1"}"#).unwrap();
        let mut options = options(path.clone());
        options.check_dns = true;

        let report = run_with(
            &options,
            |host| host != "q.moon-1.amazonaws.com",
            |_, _| Ok(()),
        )
        .unwrap();

        assert_eq!(report.changes.len(), 1);
        assert!(report.changes[0].reason.contains("DNS lookup failed"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn apply_requires_service_stopped_confirmation() {
        let path = temp_path("running");
        std::fs::write(&path, r#"{"region":"eu-north-1"}"#).unwrap();
        let mut options = options(path.clone());
        options.apply = true;

        let error = run_with(&options, |_| true, |_, _| Ok(()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("--service-stopped"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_failure_keeps_original_and_backup() {
        let path = temp_path("write-failure");
        let original = r#"{"region":"eu-north-1"}"#;
        std::fs::write(&path, original).unwrap();
        let mut options = options(path.clone());
        options.apply = true;
        options.service_stopped = true;

        let error = run_with(
            &options,
            |_| true,
            |path, _| {
                std::fs::remove_file(path)?;
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("original restored"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("{file_name}.bak.")))
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(&backups[0]);
    }
}
