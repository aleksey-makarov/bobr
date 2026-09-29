use crate::error::ExecutionError;
use bobr_core::ProgressPolicy;
use bobr_repo::{RepositoryTlsConfig, TrustedKeys};
use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

/// The request format this build of `bobr` accepts.
///
/// It is the compatibility contract between `bobr` and whoever writes requests:
/// a recipe layer emitting a different schema is talking to the wrong version.
/// `bobr --version` reports it, so a caller can compare before building rather
/// than discovering the mismatch in the parse error.
pub const REQUEST_SCHEMA: &str = "bobr-request-v6";

/// Schema marker for the request format. It deserializes only from the exact
/// schema string, so the format version is enforced declaratively at parse
/// time and never needs to live as data on `Request`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestSchemaV6;

impl<'de> Deserialize<'de> for RequestSchemaV6 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            REQUEST_SCHEMA => Ok(RequestSchemaV6),
            other => Err(D::Error::custom(format!(
                "unsupported request schema '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    pub(crate) name: String,
    pub(crate) capability: ProviderCapability,
    pub(crate) backend: ProviderBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderCapability {
    Mappings,
    Content,
}

impl ProviderCapability {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Mappings => "mappings",
            Self::Content => "content",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProviderBackend {
    Local {
        store: PathBuf,
        #[serde(default)]
        transfer: Option<LocalTransferPolicy>,
    },
    Remote {
        master_url: Url,
        trusted_keys: Vec<PathBuf>,
        #[serde(default)]
        ca_bundle: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalTransferPolicy {
    Hardlink,
    Copy,
}

impl LocalTransferPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Secondaries {
    #[serde(default)]
    pub(crate) repository_cache: PathBuf,
    #[serde(default)]
    pub(crate) providers: Vec<ProviderConfig>,
}

/// A parsed unified Realizer request.
///
/// The request names the working store and run, an ordered non-empty goal set,
/// the complete recipe-node graph, acquisition limits, and optional secondary
/// providers. Construct it with [`Request::parse_json`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    // Validated at deserialization via RequestSchemaV6; never read afterwards.
    #[allow(dead_code)]
    pub(crate) schema: RequestSchemaV6,
    pub(crate) store: PathBuf,
    pub(crate) logs: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) run_id: String,
    pub(crate) quiet: Option<bool>,
    pub(crate) jobs: Option<usize>,
    #[serde(default)]
    pub(crate) progress: ProgressPolicy,
    #[serde(default)]
    pub(crate) limits: bobr_source::acquisition::Limits,
    #[serde(default)]
    pub(crate) secondaries: Secondaries,
    pub(crate) goals: Vec<String>,
    pub(crate) nodes: BTreeMap<String, Value>,
}

impl Request {
    /// Parses and validates a request from its JSON encoding: enforces the
    /// request schema, ordered goals, secondary capabilities, and node shapes.
    /// Returns
    /// [`ExecutionError::RequestLoad`] on malformed input.
    pub fn parse_json(bytes: &[u8]) -> Result<Self, ExecutionError> {
        let mut request: Request = serde_json::from_slice(bytes).map_err(|error| {
            ExecutionError::RequestLoad(format!("failed to decode request JSON value: {error}"))
        })?;
        validate_nodes(&request.nodes, "$.nodes")?;
        validate_goals(&request.goals, &request.nodes)?;
        validate_limits(request.jobs, &request.limits)?;
        request
            .progress
            .validate()
            .map_err(ExecutionError::InvalidRequest)?;
        if !request.store.is_absolute() {
            return Err(ExecutionError::InvalidRequest(format!(
                "working store path must be absolute: '{}'",
                request.store.display()
            )));
        }
        if request.secondaries.repository_cache.as_os_str().is_empty() {
            request.secondaries.repository_cache = request.store.join("repository-cache");
        }
        validate_secondaries(&request.store, &mut request.secondaries)?;
        Ok(request)
    }
}

/// Validates that every node is an object. Per-node fields are interpreted
/// later, during graph planning.
fn validate_nodes(nodes: &BTreeMap<String, Value>, path: &str) -> Result<(), ExecutionError> {
    for (node_id, node_value) in nodes {
        if !node_value.is_object() {
            return Err(ExecutionError::RequestLoad(format!(
                "{path}.{node_id}: expected request object"
            )));
        }
    }
    Ok(())
}

fn validate_goals(goals: &[String], nodes: &BTreeMap<String, Value>) -> Result<(), ExecutionError> {
    if goals.is_empty() {
        return Err(ExecutionError::InvalidRequest(
            "request requires at least one goal".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for goal in goals {
        if !seen.insert(goal) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request contains duplicate goal node id '{goal}'"
            )));
        }
        if !nodes.contains_key(goal) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request goal references unknown node id '{goal}'"
            )));
        }
    }
    Ok(())
}

fn validate_limits(
    jobs: Option<usize>,
    limits: &bobr_source::acquisition::Limits,
) -> Result<(), ExecutionError> {
    if jobs == Some(0) {
        return Err(ExecutionError::InvalidRequest(
            "request 'jobs' must be greater than zero".to_string(),
        ));
    }
    for (name, value) in [
        ("limits.per_host_default", limits.per_host_default),
        ("limits.max_connections", limits.max_connections),
        ("limits.max_local_jobs", limits.max_local_jobs),
    ] {
        if value == Some(0) {
            return Err(ExecutionError::InvalidRequest(format!(
                "request '{name}' must be greater than zero"
            )));
        }
    }
    if let Some((host, _)) = limits.per_host.iter().find(|(_, value)| **value == 0) {
        return Err(ExecutionError::InvalidRequest(format!(
            "request 'limits.per_host.{host}' must be greater than zero"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BackendIdentity {
    Local(PathBuf),
    Remote(Url),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteSettings {
    trusted_keys: Vec<PathBuf>,
    ca_bundle: Option<PathBuf>,
}

fn validate_secondaries(store: &Path, secondaries: &mut Secondaries) -> Result<(), ExecutionError> {
    if !secondaries.repository_cache.is_absolute() {
        return Err(ExecutionError::InvalidRequest(format!(
            "repository cache path must be absolute: '{}'",
            secondaries.repository_cache.display()
        )));
    }
    let mut names: HashMap<(ProviderCapability, String), BackendIdentity> = HashMap::new();
    let mut backends: HashMap<(ProviderCapability, BackendIdentity), String> = HashMap::new();
    let mut backend_by_name: HashMap<String, BackendIdentity> = HashMap::new();
    let mut remote_settings: HashMap<Url, RemoteSettings> = HashMap::new();
    for entry in &mut secondaries.providers {
        if entry.name.trim().is_empty() {
            return Err(ExecutionError::InvalidRequest(
                "secondary provider name must not be empty".to_string(),
            ));
        }
        let identity = match &mut entry.backend {
            ProviderBackend::Local {
                store: provider_store,
                transfer,
            } => {
                if !provider_store.is_absolute() {
                    return Err(ExecutionError::InvalidRequest(format!(
                        "local provider '{}' store path must be absolute: '{}'",
                        entry.name,
                        provider_store.display()
                    )));
                }
                if *provider_store == store {
                    return Err(ExecutionError::InvalidRequest(format!(
                        "local provider '{}' is the working store itself",
                        entry.name
                    )));
                }
                match (entry.capability, *transfer) {
                    (ProviderCapability::Mappings, None)
                    | (ProviderCapability::Content, Some(_)) => {}
                    (ProviderCapability::Mappings, Some(_)) => {
                        return Err(ExecutionError::InvalidRequest(format!(
                            "mapping provider '{}' must not specify local transfer mode",
                            entry.name
                        )));
                    }
                    (ProviderCapability::Content, None) => {
                        return Err(ExecutionError::InvalidRequest(format!(
                            "content provider '{}' requires a local transfer mode",
                            entry.name
                        )));
                    }
                }
                BackendIdentity::Local(provider_store.clone())
            }
            ProviderBackend::Remote {
                master_url,
                trusted_keys,
                ca_bundle,
            } => {
                validate_master_url(&entry.name, master_url)?;
                if trusted_keys.is_empty() {
                    return Err(ExecutionError::InvalidRequest(format!(
                        "remote provider '{}' requires at least one trusted key",
                        entry.name
                    )));
                }
                for key in trusted_keys.iter_mut() {
                    *key = canonical_regular_file(&entry.name, "trusted key", key)?;
                }
                trusted_keys.sort();
                TrustedKeys::from_files(trusted_keys).map_err(|error| {
                    ExecutionError::InvalidRequest(format!(
                        "remote provider '{}' has invalid trusted keys: {error}",
                        entry.name
                    ))
                })?;
                if let Some(path) = ca_bundle {
                    *path = canonical_regular_file(&entry.name, "CA bundle", path)?;
                    RepositoryTlsConfig::from_ca_bundle(path).map_err(|error| {
                        ExecutionError::InvalidRequest(format!(
                            "remote provider '{}' has an invalid CA bundle: {error}",
                            entry.name
                        ))
                    })?;
                }
                let settings = RemoteSettings {
                    trusted_keys: trusted_keys.clone(),
                    ca_bundle: ca_bundle.clone(),
                };
                if let Some(previous) = remote_settings.insert(master_url.clone(), settings.clone())
                    && previous != settings
                {
                    return Err(ExecutionError::InvalidRequest(format!(
                        "remote provider '{}' repeats master URL '{}' with different trust or transport settings",
                        entry.name, master_url
                    )));
                }
                BackendIdentity::Remote(master_url.clone())
            }
        };
        let name_key = (entry.capability, entry.name.clone());
        if names.insert(name_key, identity.clone()).is_some() {
            return Err(ExecutionError::InvalidRequest(format!(
                "duplicate {} provider name '{}'",
                entry.capability.as_str(),
                entry.name
            )));
        }
        let backend_key = (entry.capability, identity.clone());
        if let Some(previous_name) = backends.insert(backend_key, entry.name.clone()) {
            return Err(ExecutionError::InvalidRequest(format!(
                "{} providers '{previous_name}' and '{}' repeat one physical backend",
                entry.capability.as_str(),
                entry.name
            )));
        }
        if let Some(previous) = backend_by_name.insert(entry.name.clone(), identity.clone())
            && previous != identity
            && !matches!(
                (&previous, &identity),
                (BackendIdentity::Local(_), BackendIdentity::Local(_))
            )
        {
            return Err(ExecutionError::InvalidRequest(format!(
                "complementary providers named '{}' use different physical backends",
                entry.name
            )));
        }
    }
    Ok(())
}

fn validate_master_url(name: &str, url: &Url) -> Result<(), ExecutionError> {
    if url.scheme() != "https"
        || url.cannot_be_a_base()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().is_empty()
    {
        return Err(ExecutionError::InvalidRequest(format!(
            "remote provider '{name}' master URL must be an absolute HTTPS URL without credentials, query, or fragment"
        )));
    }
    Ok(())
}

fn canonical_regular_file(
    provider: &str,
    description: &str,
    path: &Path,
) -> Result<PathBuf, ExecutionError> {
    if !path.is_absolute() {
        return Err(ExecutionError::InvalidRequest(format!(
            "remote provider '{provider}' {description} path must be absolute: '{}'",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        ExecutionError::InvalidRequest(format!(
            "remote provider '{provider}' cannot resolve {description} '{}': {error}",
            path.display()
        ))
    })?;
    if !canonical.is_file() {
        return Err(ExecutionError::InvalidRequest(format!(
            "remote provider '{provider}' {description} is not a regular file: '{}'",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_names_the_run_and_its_directories() {
        let request = Request::parse_json(
            json!({
                "schema": "bobr-request-v6",
                "store": "/store",
                "logs": "/logs/run",
                "work": "/work/run",
                "run_id": "260803120000",
                "goals": ["root"],
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();

        assert_eq!(request.store, PathBuf::from("/store"));
        assert_eq!(request.logs, PathBuf::from("/logs/run"));
        assert_eq!(request.work, PathBuf::from("/work/run"));
        assert_eq!(request.run_id, "260803120000");
    }

    #[test]
    fn request_without_a_run_is_rejected() {
        let error = Request::parse_json(
            json!({
                "schema": "bobr-request-v6",
                "store": "/store",
                "goals": ["root"],
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("missing field `logs`"),
            "{error}"
        );
    }

    #[test]
    fn the_previous_request_schema_is_rejected() {
        let error = Request::parse_json(
            json!({
                "schema": "bobr-request-v5",
                "store": "/store",
                "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported request schema 'bobr-request-v5'"),
            "{error}"
        );
    }

    #[test]
    fn progress_policy_is_typed_defaulted_and_validated() {
        let base = json!({
            "schema": "bobr-request-v6",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "goals": ["root"],
            "nodes": { "root": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        let automatic = Request::parse_json(&serde_json::to_vec(&base).unwrap()).unwrap();
        assert_eq!(automatic.progress, ProgressPolicy::Auto);

        let mut summary = base.clone();
        summary["progress"] = json!({ "mode": "summary" });
        assert_eq!(
            Request::parse_json(&serde_json::to_vec(&summary).unwrap())
                .unwrap()
                .progress,
            ProgressPolicy::Summary
        );

        let mut fixed = base.clone();
        fixed["progress"] = json!({ "mode": "fixed", "max_lines": 8 });
        assert_eq!(
            Request::parse_json(&serde_json::to_vec(&fixed).unwrap())
                .unwrap()
                .progress,
            ProgressPolicy::Fixed { max_lines: 8 }
        );

        fixed["progress"]["max_lines"] = json!(3);
        assert!(
            Request::parse_json(&serde_json::to_vec(&fixed).unwrap())
                .unwrap_err()
                .to_string()
                .contains("at least 4")
        );

        let mut misspelled = base;
        misspelled["progress"] = json!({ "mode": "auto", "max_lines": 8 });
        assert!(
            Request::parse_json(&serde_json::to_vec(&misspelled).unwrap())
                .unwrap_err()
                .to_string()
                .contains("only in fixed mode")
        );
    }

    #[test]
    fn request_requires_nonempty_known_unique_goals() {
        let base = json!({
            "schema": "bobr-request-v6",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "nodes": { "node": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        let mut empty = base.clone();
        empty["goals"] = json!([]);
        assert!(Request::parse_json(&serde_json::to_vec(&empty).unwrap()).is_err());
        let mut unknown = base.clone();
        unknown["goals"] = json!(["missing"]);
        assert!(Request::parse_json(&serde_json::to_vec(&unknown).unwrap()).is_err());
        let mut duplicate = base;
        duplicate["goals"] = json!(["node", "node"]);
        assert!(Request::parse_json(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    }

    #[test]
    fn request_validates_limits_and_normalized_local_providers() {
        let request = json!({
            "schema": "bobr-request-v6",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "goals": ["node"],
            "limits": {
                "per_host_default": 2,
                "per_host": { "example.test": 1 },
                "max_connections": 8,
                "max_local_jobs": 3
            },
            "secondaries": {
                "repository_cache": "/store/repository-cache",
                "providers": [
                    {
                        "name": "old",
                        "capability": "mappings",
                        "backend": { "kind": "local", "store": "/old" }
                    },
                    {
                        "name": "old",
                        "capability": "content",
                        "backend": {
                            "kind": "local",
                            "store": "/old",
                            "transfer": "hardlink"
                        }
                    }
                ]
            },
            "nodes": { "node": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        });
        Request::parse_json(&serde_json::to_vec(&request).unwrap()).unwrap();

        let mut copy = request.clone();
        copy["secondaries"]["providers"][1]["backend"]["transfer"] = json!("copy");
        Request::parse_json(&serde_json::to_vec(&copy).unwrap()).unwrap();

        let mut zero = request.clone();
        zero["limits"]["max_local_jobs"] = json!(0);
        assert!(
            Request::parse_json(&serde_json::to_vec(&zero).unwrap())
                .unwrap_err()
                .to_string()
                .contains("max_local_jobs")
        );
        let mut relative = request.clone();
        relative["secondaries"]["providers"][0]["backend"]["store"] = json!("relative");
        assert!(
            Request::parse_json(&serde_json::to_vec(&relative).unwrap())
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
        let mut relative_cache = request.clone();
        relative_cache["secondaries"]["repository_cache"] = json!("relative-cache");
        assert!(
            Request::parse_json(&serde_json::to_vec(&relative_cache).unwrap())
                .unwrap_err()
                .to_string()
                .contains("repository cache path must be absolute")
        );
        let mut duplicate_name = request.clone();
        duplicate_name["secondaries"]["providers"] = json!([
            {
                "name": "old", "capability": "mappings",
                "backend": { "kind": "local", "store": "/one" }
            },
            {
                "name": "old", "capability": "mappings",
                "backend": { "kind": "local", "store": "/two" }
            }
        ]);
        assert!(
            Request::parse_json(&serde_json::to_vec(&duplicate_name).unwrap())
                .unwrap_err()
                .to_string()
                .contains("duplicate mappings provider name")
        );

        let mut duplicate_root = request.clone();
        duplicate_root["secondaries"]["providers"] = json!([
            {
                "name": "one", "capability": "content",
                "backend": { "kind": "local", "store": "/old", "transfer": "hardlink" }
            },
            {
                "name": "two", "capability": "content",
                "backend": { "kind": "local", "store": "/old", "transfer": "copy" }
            }
        ]);
        assert!(
            Request::parse_json(&serde_json::to_vec(&duplicate_root).unwrap())
                .unwrap_err()
                .to_string()
                .contains("repeat one physical backend")
        );

        let mut working_alias = request.clone();
        working_alias["secondaries"]["providers"][0]["backend"]["store"] = json!("/store");
        assert!(
            Request::parse_json(&serde_json::to_vec(&working_alias).unwrap())
                .unwrap_err()
                .to_string()
                .contains("working store itself")
        );

        let mut unknown_transfer = request.clone();
        unknown_transfer["secondaries"]["providers"][1]["backend"]["transfer"] = json!("auto");
        assert!(
            Request::parse_json(&serde_json::to_vec(&unknown_transfer).unwrap())
                .unwrap_err()
                .to_string()
                .contains("unknown variant `auto`")
        );

        let mut unknown_field = request.clone();
        unknown_field["secondaries"]["providers"][0]["backend"]["priority"] = json!(1);
        assert!(
            Request::parse_json(&serde_json::to_vec(&unknown_field).unwrap())
                .unwrap_err()
                .to_string()
                .contains("unknown field `priority`")
        );

        let mut mapping_transfer = request.clone();
        mapping_transfer["secondaries"]["providers"][0]["backend"]["transfer"] = json!("hardlink");
        assert!(
            Request::parse_json(&serde_json::to_vec(&mapping_transfer).unwrap())
                .unwrap_err()
                .to_string()
                .contains("must not specify local transfer mode")
        );

        let mut missing_transfer = request;
        missing_transfer["secondaries"]["providers"][1]["backend"]
            .as_object_mut()
            .unwrap()
            .remove("transfer");
        assert!(
            Request::parse_json(&serde_json::to_vec(&missing_transfer).unwrap())
                .unwrap_err()
                .to_string()
                .contains("requires a local transfer mode")
        );
    }

    fn write_test_public_key(path: &Path) {
        let mut der = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        der.extend_from_slice(&[
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ]);
        fs::write(path, der).unwrap();
    }

    fn remote_request(key: &Path) -> Value {
        json!({
            "schema": "bobr-request-v6",
            "store": "/store",
            "logs": "/logs/run",
            "work": "/work/run",
            "run_id": "run",
            "goals": ["node"],
            "secondaries": {
                "repository_cache": "/store/repository-cache",
                "providers": [{
                    "name": "remote",
                    "capability": "mappings",
                    "backend": {
                        "kind": "remote",
                        "master_url": "https://EXAMPLE.test:443/repository/master",
                        "trusted_keys": [key]
                    }
                }]
            },
            "nodes": { "node": { "name": "hello", "tag": "Group", "config": {}, "inputs": {} } }
        })
    }

    #[test]
    fn remote_provider_paths_keys_and_urls_are_validated_and_normalized() {
        let temp = tempfile::tempdir().unwrap();
        let key = temp.path().join("key.pem");
        write_test_public_key(&key);
        let request =
            Request::parse_json(&serde_json::to_vec(&remote_request(&key)).unwrap()).unwrap();
        let ProviderBackend::Remote {
            master_url,
            trusted_keys,
            ..
        } = &request.secondaries.providers[0].backend
        else {
            panic!("expected remote backend");
        };
        assert_eq!(
            master_url.as_str(),
            "https://example.test/repository/master"
        );
        assert_eq!(trusted_keys, &[key.canonicalize().unwrap()]);

        for invalid in [
            "http://example.test/master",
            "https://user@example.test/master",
            "https://example.test/master?version=1",
            "https://example.test/master#current",
        ] {
            let mut value = remote_request(&key);
            value["secondaries"]["providers"][0]["backend"]["master_url"] = json!(invalid);
            assert!(
                Request::parse_json(&serde_json::to_vec(&value).unwrap())
                    .unwrap_err()
                    .to_string()
                    .contains("master URL")
            );
        }

        let mut no_keys = remote_request(&key);
        no_keys["secondaries"]["providers"][0]["backend"]["trusted_keys"] = json!([]);
        assert!(
            Request::parse_json(&serde_json::to_vec(&no_keys).unwrap())
                .unwrap_err()
                .to_string()
                .contains("at least one trusted key")
        );

        let mut relative_key = remote_request(&key);
        relative_key["secondaries"]["providers"][0]["backend"]["trusted_keys"] = json!(["key.der"]);
        assert!(
            Request::parse_json(&serde_json::to_vec(&relative_key).unwrap())
                .unwrap_err()
                .to_string()
                .contains("trusted key path must be absolute")
        );

        let ca = temp.path().join("invalid-ca.pem");
        fs::write(&ca, b"not a certificate\n").unwrap();
        let mut invalid_ca = remote_request(&key);
        invalid_ca["secondaries"]["providers"][0]["backend"]["ca_bundle"] = json!(ca);
        assert!(
            Request::parse_json(&serde_json::to_vec(&invalid_ca).unwrap())
                .unwrap_err()
                .to_string()
                .contains("invalid CA bundle")
        );
    }

    #[test]
    fn provider_uniqueness_is_per_capability_and_physical_backend() {
        let temp = tempfile::tempdir().unwrap();
        let key = temp.path().join("key.der");
        write_test_public_key(&key);
        let mut complementary = remote_request(&key);
        let mut content = complementary["secondaries"]["providers"][0].clone();
        content["capability"] = json!("content");
        complementary["secondaries"]["providers"]
            .as_array_mut()
            .unwrap()
            .push(content);
        Request::parse_json(&serde_json::to_vec(&complementary).unwrap()).unwrap();

        let mut duplicate_backend = complementary.clone();
        duplicate_backend["secondaries"]["providers"][1]["name"] = json!("mirror");
        duplicate_backend["secondaries"]["providers"][1]["capability"] = json!("mappings");
        assert!(
            Request::parse_json(&serde_json::to_vec(&duplicate_backend).unwrap())
                .unwrap_err()
                .to_string()
                .contains("repeat one physical backend")
        );

        let mut mismatched_name = complementary;
        mismatched_name["secondaries"]["providers"][1]["backend"]["master_url"] =
            json!("https://other.example/master");
        assert!(
            Request::parse_json(&serde_json::to_vec(&mismatched_name).unwrap())
                .unwrap_err()
                .to_string()
                .contains("use different physical backends")
        );
    }

    #[test]
    fn old_nested_root_shape_is_rejected() {
        let old_shape = json!({
            "name": "hello",
            "tag": "Tree",
            "config": {
                "tree": {
                    "entries": [{
                        "type": "file",
                        "path": "hello.txt",
                        "text": "hi",
                        "executable": false
                    }]
                }
            },
            "inputs": {}
        });

        let error =
            Request::parse_json(serde_json::to_vec(&old_shape).unwrap().as_slice()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to decode request JSON value"),
            "{error}"
        );
    }
}
