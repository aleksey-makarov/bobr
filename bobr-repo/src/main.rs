//! Administrative command-line frontend for Bobr remote repositories.

#![allow(missing_docs)]

mod status_report;

use async_trait::async_trait;
use bobr_repo::{
    BucketPresence, ImmutableUpload, MAX_MASTER_BYTES, Master, MasterHash, PublicationMetadata,
    PublicationMode, PublicationState, RepositoryError, RepositoryReader, RepositoryTlsConfig,
    RepositoryTransport, S3Location, S3Object, S3Repository, S3RepositoryTransport, Slot,
    SlotContents, StoredKey, TrustedKeys, encode_preferred_fs_files, encode_preferred_object,
};
use bobr_runtime::runtime_provider::{RuntimeProvider, runtime_provider_for_current_process};
use bobr_store::{ReadOnlyStore, StoreInventory};
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::{NamedTempFile, TempDir};
use url::Url;

use status_report::{RepositoryState, StatusReport};

const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const OCTET_STREAM: &str = "application/octet-stream";
const OBJECT_MEDIA_TYPE: &str = "application/vnd.bobr.repository-object+cbor";
const FS_FILE_MEDIA_TYPE: &str = "application/vnd.bobr.repository-fs-file+cbor";
const DEFAULT_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const ENCODING_BATCH_SIZE: usize = 64;
const UPLOAD_CONCURRENCY: usize = 16;

#[tokio::main]
async fn main() -> ExitCode {
    if let Some(code) = run_runtime_worker_if_requested() {
        return code;
    }
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error[bobr-repo]: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), RepositoryError> {
    let mut args = Arguments::new(env::args_os().skip(1));
    let Some(command) = args.next_utf8()? else {
        return Err(usage_error());
    };
    if command == "--version" || command == "-V" {
        println!("bobr-repo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    match command.as_str() {
        "init" => init(InitArgs::parse(&mut args)?).await,
        "prepare" => prepare(PrepareArgs::parse(&mut args)?).await,
        "publish" => publish(PublishArgs::parse(&mut args)?).await,
        "status" => status(StatusArgs::parse(&mut args)?).await,
        "gc" => gc(GcArgs::parse(&mut args)?).await,
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => Err(usage_error()),
    }
}

#[derive(Debug)]
struct InitArgs {
    repository: String,
    ca_bundle: Option<PathBuf>,
}

impl InitArgs {
    fn parse(args: &mut Arguments) -> Result<Self, RepositoryError> {
        let mut repository = None;
        let mut ca_bundle = None;
        while let Some(flag) = args.next_utf8()? {
            match flag.as_str() {
                "--repository" => repository = Some(args.value(&flag)?),
                "--ca-bundle" => ca_bundle = Some(args.value_path(&flag)?),
                _ => {
                    return Err(RepositoryError::new(format!(
                        "unknown init option '{flag}'"
                    )));
                }
            }
        }
        Ok(Self {
            repository: required(repository, "--repository")?,
            ca_bundle,
        })
    }
}

#[derive(Debug)]
struct PrepareArgs {
    store: PathBuf,
    repository: String,
    master_url: Url,
    trusted_keys: Vec<PathBuf>,
    data_base_url: Option<Url>,
    cache: Option<PathBuf>,
    ca_bundle: Option<PathBuf>,
    action: PrepareAction,
    retention: Option<u64>,
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepareAction {
    Append,
    AddSlot,
    Rotate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum PrepareResultKind {
    Candidate,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct PrepareResult {
    result: PrepareResultKind,
}

#[derive(Debug)]
enum ExistingPreparePlan {
    Unchanged,
    Candidate {
        metadata: Box<PublicationMetadata>,
        upload_immutable: bool,
    },
}

impl PrepareArgs {
    fn parse(args: &mut Arguments) -> Result<Self, RepositoryError> {
        let mut store = None;
        let mut repository = None;
        let mut master_url = None;
        let mut trusted_keys = Vec::new();
        let mut data_base_url = None;
        let mut cache = None;
        let mut ca_bundle = None;
        let mut action = PrepareAction::Append;
        let mut action_seen = false;
        let mut retention = None;
        let mut output = None;
        while let Some(flag) = args.next_utf8()? {
            match flag.as_str() {
                "--store" => store = Some(args.value_path(&flag)?),
                "--repository" => repository = Some(args.value(&flag)?),
                "--master-url" => {
                    master_url = Some(parse_https(&args.value(&flag)?, "master URL")?)
                }
                "--trusted-key" => trusted_keys.push(args.value_path(&flag)?),
                "--data-base-url" => {
                    data_base_url = Some(parse_https(&args.value(&flag)?, "data base URL")?)
                }
                "--cache" => cache = Some(args.value_path(&flag)?),
                "--ca-bundle" => ca_bundle = Some(args.value_path(&flag)?),
                "--append" => set_action(&mut action, &mut action_seen, PrepareAction::Append)?,
                "--add-slot" => set_action(&mut action, &mut action_seen, PrepareAction::AddSlot)?,
                "--rotate" => set_action(&mut action, &mut action_seen, PrepareAction::Rotate)?,
                "--retention" => retention = Some(parse_duration(&args.value(&flag)?)?),
                "--output" => output = Some(args.value_path(&flag)?),
                _ => {
                    return Err(RepositoryError::new(format!(
                        "unknown prepare option '{flag}'"
                    )));
                }
            }
        }
        if action == PrepareAction::AddSlot && retention.is_some() {
            return Err(RepositoryError::new(
                "--add-slot cannot be combined with --retention",
            ));
        }
        Ok(Self {
            store: required(store, "--store")?,
            repository: required(repository, "--repository")?,
            master_url: required(master_url, "--master-url")?,
            trusted_keys,
            data_base_url,
            cache,
            ca_bundle,
            action,
            retention,
            output: required(output, "--output")?,
        })
    }
}

#[derive(Debug)]
struct PublishArgs {
    candidate: PathBuf,
    repository: String,
    signing_key: PathBuf,
    trusted_keys: Vec<PathBuf>,
    ca_bundle: Option<PathBuf>,
    yes: bool,
}

impl PublishArgs {
    fn parse(args: &mut Arguments) -> Result<Self, RepositoryError> {
        let mut candidate = None;
        let mut repository = None;
        let mut signing_key = None;
        let mut trusted_keys = Vec::new();
        let mut ca_bundle = None;
        let mut yes = false;
        while let Some(flag) = args.next_utf8()? {
            match flag.as_str() {
                "--candidate" => candidate = Some(args.value_path(&flag)?),
                "--repository" => repository = Some(args.value(&flag)?),
                "--signing-key" => signing_key = Some(args.value_path(&flag)?),
                "--trusted-key" => trusted_keys.push(args.value_path(&flag)?),
                "--ca-bundle" => ca_bundle = Some(args.value_path(&flag)?),
                "--yes" => yes = true,
                _ => {
                    return Err(RepositoryError::new(format!(
                        "unknown publish option '{flag}'"
                    )));
                }
            }
        }
        Ok(Self {
            candidate: required(candidate, "--candidate")?,
            repository: required(repository, "--repository")?,
            signing_key: required(signing_key, "--signing-key")?,
            trusted_keys,
            ca_bundle,
            yes,
        })
    }
}

#[derive(Debug)]
struct StatusArgs {
    master_url: Url,
    trusted_keys: Vec<PathBuf>,
    cache: Option<PathBuf>,
    ca_bundle: Option<PathBuf>,
    compact: bool,
    repository: Option<String>,
    scan_storage: bool,
}

impl StatusArgs {
    fn parse(args: &mut Arguments) -> Result<Self, RepositoryError> {
        let mut master_url = None;
        let mut trusted_keys = Vec::new();
        let mut cache = None;
        let mut ca_bundle = None;
        let mut compact = false;
        let mut repository = None;
        let mut scan_storage = false;
        while let Some(flag) = args.next_utf8()? {
            match flag.as_str() {
                "--master-url" => {
                    master_url = Some(parse_https(&args.value(&flag)?, "master URL")?)
                }
                "--trusted-key" => trusted_keys.push(args.value_path(&flag)?),
                "--cache" => cache = Some(args.value_path(&flag)?),
                "--ca-bundle" => ca_bundle = Some(args.value_path(&flag)?),
                "--compact" => compact = true,
                "--repository" => repository = Some(args.value(&flag)?),
                "--scan-storage" => scan_storage = true,
                _ => {
                    return Err(RepositoryError::new(format!(
                        "unknown status option '{flag}'"
                    )));
                }
            }
        }
        if scan_storage && repository.is_none() {
            return Err(RepositoryError::new("--scan-storage requires --repository"));
        }
        Ok(Self {
            master_url: required(master_url, "--master-url")?,
            trusted_keys,
            cache,
            ca_bundle,
            compact,
            repository,
            scan_storage,
        })
    }
}

#[derive(Debug)]
struct GcArgs {
    repository: String,
    master_url: Url,
    trusted_keys: Vec<PathBuf>,
    cache: Option<PathBuf>,
    ca_bundle: Option<PathBuf>,
    dry_run: bool,
}

impl GcArgs {
    fn parse(args: &mut Arguments) -> Result<Self, RepositoryError> {
        let mut repository = None;
        let mut master_url = None;
        let mut trusted_keys = Vec::new();
        let mut cache = None;
        let mut ca_bundle = None;
        let mut dry_run = false;
        while let Some(flag) = args.next_utf8()? {
            match flag.as_str() {
                "--repository" => repository = Some(args.value(&flag)?),
                "--master-url" => {
                    master_url = Some(parse_https(&args.value(&flag)?, "master URL")?)
                }
                "--trusted-key" => trusted_keys.push(args.value_path(&flag)?),
                "--cache" => cache = Some(args.value_path(&flag)?),
                "--ca-bundle" => ca_bundle = Some(args.value_path(&flag)?),
                "--dry-run" => dry_run = true,
                _ => return Err(RepositoryError::new(format!("unknown gc option '{flag}'"))),
            }
        }
        Ok(Self {
            repository: required(repository, "--repository")?,
            master_url: required(master_url, "--master-url")?,
            trusted_keys,
            cache,
            ca_bundle,
            dry_run,
        })
    }
}

async fn init(args: InitArgs) -> Result<(), RepositoryError> {
    let tls_config = load_tls_config(args.ca_bundle.as_deref())?;
    let location = S3Location::parse(&args.repository)?;
    location.require_bucket_root()?;
    let repository = S3Repository::from_environment(location, &tls_config).await?;
    let initialized = repository.initialize_dedicated_bucket().await?;
    let bucket = if initialized.created {
        "created"
    } else {
        "already existed"
    };
    let policy = if initialized.policy_updated {
        "installed"
    } else {
        "already current"
    };
    eprintln!("bucket {bucket}; public-read policy {policy}");
    if !initialized.public_access_block_supported {
        eprintln!("bucket Public Access Block is not supported by this S3 implementation");
    }
    Ok(())
}

async fn prepare(args: PrepareArgs) -> Result<(), RepositoryError> {
    let tls_config = load_tls_config(args.ca_bundle.as_deref())?;
    let location = S3Location::parse(&args.repository)?;
    let repository = S3Repository::from_environment(location, &tls_config).await?;
    let trusted = load_trusted_keys(&args.trusted_keys)?;
    let current_object = repository.get_bytes("master", MAX_MASTER_BYTES).await?;
    let current_verified = current_object
        .as_ref()
        .map(|object| trusted.verify(&object.bytes))
        .transpose()?;
    let cache = CacheRoot::new(args.cache.as_deref())?;
    let runtime = runtime_provider_for_current_process();

    let state = if let Some(verified) = current_verified {
        if let Some(url) = &args.data_base_url
            && url != verified.master.data_base_url()
        {
            return Err(RepositoryError::new(
                "--data-base-url differs from the authenticated current master",
            ));
        }
        let transport = Arc::new(S3RepositoryTransport::new(
            repository.clone(),
            args.master_url.clone(),
            verified.master.data_base_url().clone(),
        ));
        let reader =
            RepositoryReader::new(args.master_url.clone(), trusted, cache.path(), transport)?;
        reader.publication_metadata().await?.state()?
    } else {
        if args.action != PrepareAction::Append {
            return Err(RepositoryError::new(
                "an empty repository can only be prepared with the default append action",
            ));
        }
        let data_base_url = args
            .data_base_url
            .ok_or_else(|| RepositoryError::new("an empty repository requires --data-base-url"))?;
        let inventory = scan_inventory(&args.store, runtime.clone()).await?;
        let incoming = inventory_contents(&inventory)?;
        upload_inventory(&repository, &inventory, &runtime).await?;
        let state = PublicationState::initialize(data_base_url, incoming)?;
        let metadata = state.metadata()?;
        upload_metadata(&repository, &metadata).await?;
        let result = persist_prepare_candidate(&args.output, Some(&metadata))?;
        eprintln!(
            "prepared initial repository state from {}",
            args.store.display()
        );
        print_prepare_result(result)?;
        return Ok(());
    };

    let inventory = scan_inventory(&args.store, runtime.clone()).await?;
    let incoming = inventory_contents(&inventory)?;
    let now = unix_time()?;
    let retention = args.retention.unwrap_or(DEFAULT_RETENTION_SECONDS);
    let plan = plan_existing_prepare(state, args.action, incoming, now, retention)?;
    let ExistingPreparePlan::Candidate {
        metadata,
        upload_immutable,
    } = plan
    else {
        eprintln!("active repository slot is unchanged");
        let result = persist_prepare_candidate(&args.output, None)?;
        print_prepare_result(result)?;
        return Ok(());
    };
    if upload_immutable {
        upload_inventory(&repository, &inventory, &runtime).await?;
        upload_metadata(&repository, &metadata).await?;
    }
    let result = persist_prepare_candidate(&args.output, Some(&metadata))?;
    eprintln!(
        "prepared repository candidate with {} slot state(s)",
        metadata.master.slots().len()
    );
    print_prepare_result(result)?;
    Ok(())
}

fn plan_existing_prepare(
    mut state: PublicationState,
    action: PrepareAction,
    incoming: SlotContents,
    now: u64,
    retention: u64,
) -> Result<ExistingPreparePlan, RepositoryError> {
    let pruned = state.prune_expired(now)? > 0;
    let retain_until = now
        .checked_add(retention)
        .ok_or_else(|| RepositoryError::new("retention deadline overflow"))?;

    if action == PrepareAction::Append {
        let current_metadata = state.metadata()?;
        let mut appended = state.clone();
        appended.publish(PublicationMode::AppendToActive { retain_until }, incoming)?;
        let appended_metadata = appended.metadata()?;
        if slot_metadata_equal(
            current_metadata.master.active_slot(),
            appended_metadata.master.active_slot(),
        ) {
            return if pruned {
                Ok(ExistingPreparePlan::Candidate {
                    metadata: Box::new(current_metadata),
                    upload_immutable: false,
                })
            } else {
                Ok(ExistingPreparePlan::Unchanged)
            };
        }
        return Ok(ExistingPreparePlan::Candidate {
            metadata: Box::new(appended_metadata),
            upload_immutable: true,
        });
    }

    let mode = match action {
        PrepareAction::Append => unreachable!("append handled above"),
        PrepareAction::AddSlot => PublicationMode::AddSlot,
        PrepareAction::Rotate => PublicationMode::Rotate { retain_until },
    };
    state.publish(mode, incoming)?;
    Ok(ExistingPreparePlan::Candidate {
        metadata: Box::new(state.metadata()?),
        upload_immutable: true,
    })
}

fn slot_metadata_equal(left: &Slot, right: &Slot) -> bool {
    left.build == right.build
        && left.reuse == right.reuse
        && left.object_list == right.object_list
        && left.file_list == right.file_list
}

fn print_prepare_result(result: PrepareResultKind) -> Result<(), RepositoryError> {
    println!("{}", json_compact(&PrepareResult { result })?);
    Ok(())
}

fn persist_prepare_candidate(
    path: &Path,
    metadata: Option<&PublicationMetadata>,
) -> Result<PrepareResultKind, RepositoryError> {
    let Some(metadata) = metadata else {
        return Ok(PrepareResultKind::Unchanged);
    };
    write_candidate(path, &metadata.master.encode_payload())?;
    Ok(PrepareResultKind::Candidate)
}

async fn publish(args: PublishArgs) -> Result<(), RepositoryError> {
    let candidate = fs::read(&args.candidate)?;
    if candidate.len() as u64 > MAX_MASTER_BYTES {
        return Err(RepositoryError::new(
            "candidate exceeds the master size limit",
        ));
    }
    let master = Master::decode_payload(&candidate)?;
    if master.encode_payload() != candidate {
        return Err(RepositoryError::new("candidate is not deterministic CBOR"));
    }
    let tls_config = load_tls_config(args.ca_bundle.as_deref())?;
    let repository =
        S3Repository::from_environment(S3Location::parse(&args.repository)?, &tls_config).await?;
    let trusted = load_trusted_keys(&args.trusted_keys)?;
    let current = repository.get_bytes("master", MAX_MASTER_BYTES).await?;
    let (predecessor_etag, current_master) = match (master.previous_master_hash(), current.as_ref())
    {
        (None, None) => (None, None),
        (Some(expected), Some(current)) => {
            let verified = trusted.verify(&current.bytes)?;
            if verified.signed_hash != expected {
                return Err(RepositoryError::new(
                    "candidate predecessor does not match the current master",
                ));
            }
            (Some(current.etag.clone()), Some(verified.master))
        }
        (None, Some(_)) => {
            return Err(RepositoryError::new(
                "initial candidate cannot replace an existing master",
            ));
        }
        (Some(_), None) => {
            return Err(RepositoryError::new(
                "candidate names a predecessor but the repository has no master",
            ));
        }
    };
    eprintln!(
        "{}",
        json_pretty(&publication_comparison(current_master.as_ref(), &master))?
    );
    if !args.yes {
        confirm_publication()?;
    }
    let signing_key = load_signing_key(&args.signing_key)?;
    let key_id: [u8; 32] = Sha256::digest(signing_key.verifying_key().as_bytes()).into();
    let signed = master.sign(&key_id, &signing_key)?;
    let temporary = write_temporary_bytes(&signed)?;
    repository
        .put_master(temporary.path(), predecessor_etag.as_deref())
        .await?;
    let stored = repository
        .get_bytes("master", MAX_MASTER_BYTES)
        .await?
        .ok_or_else(|| RepositoryError::new("published master disappeared"))?;
    if stored.bytes != signed {
        return Err(RepositoryError::new(
            "stored master is not byte-identical to the signed publication",
        ));
    }
    let mut own_key = TrustedKeys::default();
    own_key.insert(key_id.to_vec(), signing_key.verifying_key())?;
    own_key.verify(&stored.bytes)?;
    eprintln!("published master {}", MasterHash::digest(&stored.bytes));
    Ok(())
}

async fn status(args: StatusArgs) -> Result<(), RepositoryError> {
    let tls_config = load_tls_config(args.ca_bundle.as_deref())?;
    let trusted = load_trusted_keys(&args.trusted_keys)?;
    let cache = CacheRoot::new(args.cache.as_deref())?;
    let now = unix_time()?;
    let report = if args.scan_storage {
        let repository = S3Repository::from_environment(
            S3Location::parse(
                args.repository
                    .as_deref()
                    .expect("validated status repository"),
            )?,
            &tls_config,
        )
        .await?;
        administrative_status(&args.master_url, trusted, cache.path(), repository, now).await?
    } else {
        let reader =
            RepositoryReader::https(args.master_url.clone(), trusted, cache.path(), &tls_config)?;
        let publication = reader.publication_metadata().await?;
        StatusReport::ready(&args.master_url, &publication, None, now)?
    };
    if args.compact {
        println!("{}", json_compact(&report)?);
    } else {
        println!("{}", json_pretty(&report)?);
    }
    Ok(())
}

async fn administrative_status(
    master_url: &Url,
    trusted: TrustedKeys,
    cache_root: &Path,
    repository: impl StatusRepository,
    now: u64,
) -> Result<StatusReport, RepositoryError> {
    administrative_status_from(master_url, trusted, cache_root, &repository, now).await
}

#[async_trait]
trait StatusRepository: Send + Sync {
    async fn bucket_presence(&self) -> Result<BucketPresence, RepositoryError>;
    async fn get_master(&self) -> Result<Option<S3Object>, RepositoryError>;
    async fn list(&self) -> Result<Vec<StoredKey>, RepositoryError>;
    fn metadata_transport(
        &self,
        master_url: Url,
        data_base_url: Url,
    ) -> Arc<dyn RepositoryTransport>;
}

#[async_trait]
impl StatusRepository for S3Repository {
    async fn bucket_presence(&self) -> Result<BucketPresence, RepositoryError> {
        S3Repository::bucket_presence(self).await
    }

    async fn get_master(&self) -> Result<Option<S3Object>, RepositoryError> {
        self.get_bytes("master", MAX_MASTER_BYTES).await
    }

    async fn list(&self) -> Result<Vec<StoredKey>, RepositoryError> {
        S3Repository::list(self).await
    }

    fn metadata_transport(
        &self,
        master_url: Url,
        data_base_url: Url,
    ) -> Arc<dyn RepositoryTransport> {
        Arc::new(S3RepositoryTransport::new(
            self.clone(),
            master_url,
            data_base_url,
        ))
    }
}

async fn administrative_status_from(
    master_url: &Url,
    trusted: TrustedKeys,
    cache_root: &Path,
    repository: &impl StatusRepository,
    now: u64,
) -> Result<StatusReport, RepositoryError> {
    for attempt in 0..2 {
        if repository.bucket_presence().await? == BucketPresence::Missing {
            return Ok(StatusReport::empty(RepositoryState::Missing));
        }
        let Some(observed_master) = repository.get_master().await? else {
            if repository.bucket_presence().await? == BucketPresence::Missing {
                return Ok(StatusReport::empty(RepositoryState::Missing));
            }
            if repository.get_master().await?.is_none() {
                return Ok(StatusReport::empty(RepositoryState::Empty));
            }
            if attempt == 0 {
                continue;
            }
            return Err(RepositoryError::new(
                "repository master changed repeatedly while collecting status",
            ));
        };
        let verified = trusted.verify(&observed_master.bytes)?;
        let transport = repository
            .metadata_transport(master_url.clone(), verified.master.data_base_url().clone());
        let reader =
            RepositoryReader::new(master_url.clone(), trusted.clone(), cache_root, transport)?;
        let publication = match reader.publication_metadata().await {
            Ok(publication) => publication,
            Err(error) => {
                if repository_master_changed(repository, &observed_master.bytes).await? {
                    if attempt == 0 {
                        continue;
                    }
                    return Err(RepositoryError::new(
                        "repository master changed repeatedly while collecting status",
                    ));
                }
                return Err(error);
            }
        };
        let stored = match repository.list().await {
            Ok(stored) => stored,
            Err(error) => {
                if repository.bucket_presence().await? == BucketPresence::Missing {
                    if attempt == 0 {
                        continue;
                    }
                    return Err(RepositoryError::new(
                        "repository disappeared repeatedly while collecting status",
                    ));
                }
                return Err(error);
            }
        };
        let current = repository.get_master().await?;
        if current
            .as_ref()
            .is_some_and(|master| MasterHash::digest(&master.bytes) == publication.master_hash)
        {
            return StatusReport::ready(master_url, &publication, Some(&stored), now);
        }
        if attempt == 1 {
            return Err(RepositoryError::new(
                "repository master changed repeatedly while collecting status",
            ));
        }
    }
    unreachable!("administrative status retry loop has a fixed non-empty range")
}

async fn repository_master_changed(
    repository: &impl StatusRepository,
    observed: &[u8],
) -> Result<bool, RepositoryError> {
    Ok(repository
        .get_master()
        .await?
        .is_none_or(|current| current.bytes != observed))
}

async fn gc(args: GcArgs) -> Result<(), RepositoryError> {
    let tls_config = load_tls_config(args.ca_bundle.as_deref())?;
    let repository =
        S3Repository::from_environment(S3Location::parse(&args.repository)?, &tls_config).await?;
    let master_object = repository
        .get_bytes("master", MAX_MASTER_BYTES)
        .await?
        .ok_or_else(|| RepositoryError::new("repository master is missing"))?;
    let trusted = load_trusted_keys(&args.trusted_keys)?;
    let verified = trusted.verify(&master_object.bytes)?;
    let cache = CacheRoot::new(args.cache.as_deref())?;
    let transport = Arc::new(S3RepositoryTransport::new(
        repository.clone(),
        args.master_url.clone(),
        verified.master.data_base_url().clone(),
    ));
    let reader = RepositoryReader::new(args.master_url, trusted, cache.path(), transport)?;
    let publication = reader.publication_metadata().await?;
    let live = live_keys(&publication.metadata);
    let stored = repository.list().await?;
    let deletion = stored
        .iter()
        .filter(|entry| recognized_immutable(&entry.key) && !live.contains(&entry.key))
        .map(|entry| entry.key.clone())
        .collect::<Vec<_>>();
    let stored_keys = stored
        .iter()
        .map(|entry| entry.key.as_str())
        .collect::<HashSet<_>>();
    let missing = live
        .iter()
        .filter(|key| !stored_keys.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(RepositoryError::new(format!(
            "garbage collection aborted: {} live repository key(s) are missing; first is '{}'",
            missing.len(),
            missing[0]
        )));
    }
    let bytes = stored
        .iter()
        .filter(|entry| deletion.binary_search(&entry.key).is_ok())
        .map(|entry| entry.size)
        .sum::<u64>();
    let namespaces = stored
        .iter()
        .filter(|entry| deletion.binary_search(&entry.key).is_ok())
        .fold(BTreeMap::<String, (usize, u64)>::new(), |mut map, entry| {
            let namespace = entry.key.split('/').next().unwrap_or("other").to_owned();
            let summary = map.entry(namespace).or_default();
            summary.0 += 1;
            summary.1 += entry.size;
            map
        });
    println!(
        "{}",
        json_pretty(&json!({
            "dry_run": args.dry_run,
            "delete_keys": deletion.len(),
            "delete_bytes": bytes,
            "namespaces": namespaces.into_iter().map(|(name, (keys, bytes))| {
                (name, json!({"keys": keys, "bytes": bytes}))
            }).collect::<serde_json::Map<_, _>>(),
        }))?
    );
    if !args.dry_run {
        let latest = repository
            .get_bytes("master", MAX_MASTER_BYTES)
            .await?
            .ok_or_else(|| RepositoryError::new("repository master disappeared before GC"))?;
        if MasterHash::digest(&latest.bytes) != publication.master_hash {
            return Err(RepositoryError::new(
                "garbage collection aborted: repository master changed during analysis",
            ));
        }
        repository.delete(&deletion).await?;
    }
    Ok(())
}

async fn scan_inventory(
    path: &Path,
    runtime: RuntimeProvider,
) -> Result<StoreInventory, RepositoryError> {
    let path = fs::canonicalize(path)?;
    tokio::task::spawn_blocking(move || {
        ReadOnlyStore::open(&path)?.inventory_with_runtime(&runtime)
    })
    .await
    .map_err(|error| RepositoryError::new(format!("store inventory task failed: {error}")))?
    .map_err(|error| RepositoryError::new(error.to_string()))
}

fn inventory_contents(inventory: &StoreInventory) -> Result<SlotContents, RepositoryError> {
    SlotContents::new(
        inventory.builds.iter().copied(),
        inventory.reuses.iter().copied(),
        inventory.objects.iter().map(|object| object.hash),
        inventory.files.iter().map(|file| file.hash),
    )
}

async fn upload_inventory(
    repository: &S3Repository,
    inventory: &StoreInventory,
    runtime: &RuntimeProvider,
) -> Result<(), RepositoryError> {
    let existing = repository
        .list()
        .await?
        .into_iter()
        .map(|entry| entry.key)
        .collect::<HashSet<_>>();
    let mut uploaded = 0usize;
    for objects in inventory.objects.chunks(ENCODING_BATCH_SIZE) {
        let temporary = tempfile::tempdir()?;
        let mut uploads = Vec::new();
        for object in objects {
            let key = format!("o/{}", object.hash);
            if existing.contains(&key) {
                continue;
            }
            let encoded = encode_preferred_object(&object.path, object.hash, temporary.path())?;
            let path = temporary.path().join(object.hash.to_string());
            encoded.persist(&path).map_err(|error| error.error)?;
            uploads.push((key, path, OBJECT_MEDIA_TYPE));
        }
        uploaded += upload_batch(repository, &uploads).await?;
    }
    let missing_files = inventory
        .files
        .iter()
        .filter(|file| !existing.contains(&format!("f/{}", file.hash)))
        .cloned()
        .collect::<Vec<_>>();
    for files in missing_files.chunks(ENCODING_BATCH_SIZE) {
        let temporary = tempfile::tempdir()?;
        encode_preferred_fs_files(runtime, files, temporary.path())?;
        let uploads = files
            .iter()
            .map(|file| {
                (
                    format!("f/{}", file.hash),
                    temporary.path().join(file.hash.to_string()),
                    FS_FILE_MEDIA_TYPE,
                )
            })
            .collect::<Vec<_>>();
        uploaded += upload_batch(repository, &uploads).await?;
    }
    eprintln!("repository content: {uploaded} uploaded");
    Ok(())
}

async fn upload_batch(
    repository: &S3Repository,
    uploads: &[(String, PathBuf, &'static str)],
) -> Result<usize, RepositoryError> {
    let mut futures = stream::iter(uploads.iter().map(|(key, path, content_type)| async move {
        repository
            .put_immutable(key, path, content_type, IMMUTABLE_CACHE_CONTROL)
            .await
    }))
    .buffer_unordered(UPLOAD_CONCURRENCY);
    let mut created = 0;
    while let Some(result) = futures.next().await {
        if result? == ImmutableUpload::Created {
            created += 1;
        }
    }
    Ok(created)
}

async fn upload_metadata(
    repository: &S3Repository,
    metadata: &PublicationMetadata,
) -> Result<(), RepositoryError> {
    let temporary = tempfile::tempdir()?;
    for (namespace, hash, bytes) in metadata_values(metadata) {
        let file = temporary.path().join(format!("{namespace}-{hash}"));
        fs::write(&file, bytes)?;
        repository
            .put_immutable(
                &format!("{namespace}/{hash}"),
                &file,
                OCTET_STREAM,
                IMMUTABLE_CACHE_CONTROL,
            )
            .await?;
    }
    Ok(())
}

fn metadata_values(metadata: &PublicationMetadata) -> Vec<(&'static str, String, &[u8])> {
    let mut values = Vec::new();
    values.extend(
        metadata
            .builds
            .iter()
            .map(|(hash, bytes)| ("b", hash.to_string(), bytes.as_slice())),
    );
    values.extend(
        metadata
            .reuses
            .iter()
            .map(|(hash, bytes)| ("r", hash.to_string(), bytes.as_slice())),
    );
    values.extend(
        metadata
            .object_lists
            .iter()
            .map(|(hash, bytes)| ("lo", hash.to_string(), bytes.as_slice())),
    );
    values.extend(
        metadata
            .file_lists
            .iter()
            .map(|(hash, bytes)| ("lf", hash.to_string(), bytes.as_slice())),
    );
    values
}

fn write_candidate(path: &Path, bytes: &[u8]) -> Result<(), RepositoryError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".candidate-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn write_temporary_bytes(bytes: &[u8]) -> Result<NamedTempFile, RepositoryError> {
    let mut temporary = NamedTempFile::new()?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

fn live_keys(metadata: &PublicationMetadata) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    for slot in metadata.master.slots() {
        live.insert(format!("b/{}", slot.build));
        live.insert(format!("r/{}", slot.reuse));
        live.insert(format!("lo/{}", slot.object_list));
        live.insert(format!("lf/{}", slot.file_list));
        if let Some(bytes) = metadata.object_lists.get(&slot.object_list) {
            for hash in bytes.chunks_exact(32) {
                live.insert(format!("o/{}", hex(hash)));
            }
        }
        if let Some(bytes) = metadata.file_lists.get(&slot.file_list) {
            for hash in bytes.chunks_exact(32) {
                live.insert(format!("f/{}", hex(hash)));
            }
        }
    }
    live
}

fn recognized_immutable(key: &str) -> bool {
    ["b/", "r/", "lo/", "lf/", "o/", "f/"]
        .iter()
        .any(|namespace| key.starts_with(namespace))
}

fn publication_comparison(current: Option<&Master>, candidate: &Master) -> Value {
    let current_slots = current
        .map(|master| {
            master
                .slots()
                .iter()
                .map(|slot| (slot.serial, slot))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let candidate_slots = candidate
        .slots()
        .iter()
        .map(|slot| (slot.serial, slot))
        .collect::<BTreeMap<_, _>>();
    let added = candidate_slots
        .keys()
        .filter(|serial| !current_slots.contains_key(serial))
        .copied()
        .collect::<Vec<_>>();
    let pruned = current_slots
        .keys()
        .filter(|serial| !candidate_slots.contains_key(serial))
        .copied()
        .collect::<Vec<_>>();
    let newly_retained = current_slots
        .iter()
        .filter_map(|(serial, old)| {
            candidate_slots
                .get(serial)
                .filter(|new| old.retain_until.is_none() && new.retain_until.is_some())
                .map(|new| json!({"serial": serial, "retain_until": new.retain_until}))
        })
        .collect::<Vec<_>>();
    let modified = current_slots
        .iter()
        .filter_map(|(serial, old)| {
            candidate_slots.get(serial).and_then(|new| {
                (old.build != new.build
                    || old.reuse != new.reuse
                    || old.object_list != new.object_list
                    || old.file_list != new.file_list)
                    .then_some(*serial)
            })
        })
        .collect::<Vec<_>>();
    json!({
        "operation": if current.is_some() { "replace" } else { "create" },
        "previous_master_hash": candidate.previous_master_hash().map(|hash| hash.to_string()),
        "current_data_base_url": current.map(|master| master.data_base_url().as_str()),
        "data_base_url": candidate.data_base_url().as_str(),
        "added_slots": added,
        "newly_retained_slots": newly_retained,
        "pruned_slots": pruned,
        "modified_existing_slots": modified,
        "current_slots": current.into_iter().flat_map(Master::slots).map(slot_summary).collect::<Vec<_>>(),
        "candidate_slots": candidate.slots().iter().map(|slot| json!({
            "serial": slot.serial,
            "retain_until": slot.retain_until,
        })).collect::<Vec<_>>(),
    })
}

fn slot_summary(slot: &Slot) -> Value {
    json!({
        "serial": slot.serial,
        "retain_until": slot.retain_until,
    })
}

fn confirm_publication() -> Result<(), RepositoryError> {
    if !io::stdin().is_terminal() {
        return Err(RepositoryError::new(
            "publication requires terminal confirmation; pass --yes for noninteractive use",
        ));
    }
    eprint!("Publish this master? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        return Err(RepositoryError::new("publication cancelled"));
    }
    Ok(())
}

fn load_tls_config(path: Option<&Path>) -> Result<RepositoryTlsConfig, RepositoryError> {
    path.map_or_else(
        || Ok(RepositoryTlsConfig::default_roots()),
        RepositoryTlsConfig::from_ca_bundle,
    )
}

fn load_trusted_keys(paths: &[PathBuf]) -> Result<TrustedKeys, RepositoryError> {
    let mut trusted = TrustedKeys::default();
    for path in paths {
        let bytes = fs::read(path)?;
        let key = parse_verifying_key(&bytes).map_err(|error| {
            RepositoryError::new(format!(
                "failed to parse public key '{}': {error}",
                path.display()
            ))
        })?;
        let key_id: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        trusted.insert(key_id.to_vec(), key)?;
    }
    Ok(trusted)
}

fn parse_verifying_key(bytes: &[u8]) -> Result<VerifyingKey, String> {
    if let Ok(text) = std::str::from_utf8(bytes)
        && let Ok(key) = VerifyingKey::from_public_key_pem(text)
    {
        return Ok(key);
    }
    VerifyingKey::from_public_key_der(bytes).map_err(|error| error.to_string())
}

fn load_signing_key(path: &Path) -> Result<SigningKey, RepositoryError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(RepositoryError::new("signing key is not a regular file"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(RepositoryError::new(
            "signing key must not be accessible by group or other users",
        ));
    }
    let bytes = fs::read(path)?;
    if let Ok(text) = std::str::from_utf8(&bytes)
        && let Ok(key) = SigningKey::from_pkcs8_pem(text)
    {
        return Ok(key);
    }
    SigningKey::from_pkcs8_der(&bytes).map_err(|error| {
        RepositoryError::new(format!("failed to parse PKCS#8 signing key: {error}"))
    })
}

struct CacheRoot {
    explicit: Option<PathBuf>,
    temporary: Option<TempDir>,
}

impl CacheRoot {
    fn new(explicit: Option<&Path>) -> Result<Self, RepositoryError> {
        match explicit {
            Some(path) => {
                fs::create_dir_all(path)?;
                Ok(Self {
                    explicit: Some(fs::canonicalize(path)?),
                    temporary: None,
                })
            }
            None => Ok(Self {
                explicit: None,
                temporary: Some(tempfile::tempdir()?),
            }),
        }
    }

    fn path(&self) -> &Path {
        self.explicit
            .as_deref()
            .unwrap_or_else(|| self.temporary.as_ref().expect("cache root").path())
    }
}

struct Arguments {
    values: std::vec::IntoIter<OsString>,
}

impl Arguments {
    fn new(values: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            values: values.into_iter().collect::<Vec<_>>().into_iter(),
        }
    }

    fn next_utf8(&mut self) -> Result<Option<String>, RepositoryError> {
        self.values
            .next()
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| RepositoryError::new("argument is not UTF-8"))
            })
            .transpose()
    }

    fn value(&mut self, flag: &str) -> Result<String, RepositoryError> {
        self.next_utf8()?
            .ok_or_else(|| RepositoryError::new(format!("{flag} requires a value")))
    }

    fn value_path(&mut self, flag: &str) -> Result<PathBuf, RepositoryError> {
        self.values
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| RepositoryError::new(format!("{flag} requires a value")))
    }
}

fn required<T>(value: Option<T>, flag: &str) -> Result<T, RepositoryError> {
    value.ok_or_else(|| RepositoryError::new(format!("missing required option {flag}")))
}

fn parse_https(value: &str, name: &str) -> Result<Url, RepositoryError> {
    let url = Url::parse(value)
        .map_err(|error| RepositoryError::new(format!("invalid {name}: {error}")))?;
    if url.scheme() != "https" {
        return Err(RepositoryError::new(format!("{name} must use HTTPS")));
    }
    Ok(url)
}

fn parse_duration(value: &str) -> Result<u64, RepositoryError> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let amount = value[..split]
        .parse::<u64>()
        .map_err(|_| RepositoryError::new("invalid retention duration"))?;
    let multiplier = match &value[split..] {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => {
            return Err(RepositoryError::new(
                "retention duration must use s, m, h, or d",
            ));
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| RepositoryError::new("retention duration overflow"))
}

fn set_action(
    action: &mut PrepareAction,
    seen: &mut bool,
    value: PrepareAction,
) -> Result<(), RepositoryError> {
    if *seen {
        return Err(RepositoryError::new(
            "prepare slot actions are mutually exclusive",
        ));
    }
    *seen = true;
    *action = value;
    Ok(())
}

fn unix_time() -> Result<u64, RepositoryError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| RepositoryError::new(format!("system time precedes Unix epoch: {error}")))
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn usage_error() -> RepositoryError {
    RepositoryError::new("usage: bobr-repo <init|prepare|publish|status|gc> [options]")
}

fn json_pretty(value: &impl Serialize) -> Result<String, RepositoryError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| RepositoryError::new(format!("failed to encode JSON: {error}")))
}

fn json_compact(value: &impl Serialize) -> Result<String, RepositoryError> {
    serde_json::to_string(value)
        .map_err(|error| RepositoryError::new(format!("failed to encode JSON: {error}")))
}

fn print_usage() {
    println!("usage: bobr-repo <init|prepare|publish|status|gc> [options]");
}

fn run_runtime_worker_if_requested() -> Option<ExitCode> {
    match bobr_runtime::runtime_ns::worker_invocation_from_env() {
        Ok(Some(invocation)) => {
            let mut functions = bobr_store::runtime_functions();
            functions.extend(bobr_repo::runtime_functions());
            let result = bobr_runtime::runtime_ns::run_worker(invocation, functions);
            Some(match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error[bobr-repo-runtime-worker]: {error}");
                    ExitCode::FAILURE
                }
            })
        }
        Ok(None) => None,
        Err(error) => {
            eprintln!("error[bobr-repo-runtime-worker]: {error}");
            Some(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobr_core::{BuildKey, ObjectHash};
    use bobr_repo::{
        BuildIndexHash, FsFileListHash, MemoryTransport, ObjectListHash, ReuseIndexHash, Slot,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct FakeStatusRepository {
        presence: BucketPresence,
        masters: Mutex<VecDeque<Option<S3Object>>>,
        stored: Vec<StoredKey>,
        transports: BTreeMap<String, MemoryTransport>,
        list_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl StatusRepository for FakeStatusRepository {
        async fn bucket_presence(&self) -> Result<BucketPresence, RepositoryError> {
            Ok(self.presence)
        }

        async fn get_master(&self) -> Result<Option<S3Object>, RepositoryError> {
            let mut masters = self.masters.lock().unwrap();
            Ok(if masters.len() > 1 {
                masters.pop_front().unwrap()
            } else {
                masters.front().cloned().unwrap_or(None)
            })
        }

        async fn list(&self) -> Result<Vec<StoredKey>, RepositoryError> {
            self.list_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.stored.clone())
        }

        fn metadata_transport(
            &self,
            _master_url: Url,
            data_base_url: Url,
        ) -> Arc<dyn RepositoryTransport> {
            Arc::new(
                self.transports
                    .get(data_base_url.as_str())
                    .expect("test transport for data base URL")
                    .clone(),
            )
        }
    }

    struct RepositoryVersion {
        master: S3Object,
        data_base_url: Url,
        transport: MemoryTransport,
        stored: Vec<StoredKey>,
    }

    #[test]
    fn duration_suffixes_are_explicit() {
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert!(parse_duration("1day").is_err());
    }

    #[test]
    fn prepare_result_is_stable_compact_json() {
        assert_eq!(
            json_compact(&PrepareResult {
                result: PrepareResultKind::Candidate,
            })
            .unwrap(),
            r#"{"result":"candidate"}"#
        );
        assert_eq!(
            json_compact(&PrepareResult {
                result: PrepareResultKind::Unchanged,
            })
            .unwrap(),
            r#"{"result":"unchanged"}"#
        );
    }

    #[test]
    fn unchanged_append_preserves_the_existing_candidate() {
        let state = PublicationState::initialize(data_base_url(), contents(1, 1)).unwrap();
        let plan =
            plan_existing_prepare(state, PrepareAction::Append, contents(1, 1), 100, 50).unwrap();
        assert!(matches!(plan, ExistingPreparePlan::Unchanged));

        let temporary = tempfile::tempdir().unwrap();
        let candidate = temporary.path().join("candidate.cbor");
        fs::write(&candidate, b"previous candidate").unwrap();
        let result = persist_prepare_candidate(&candidate, None).unwrap();
        assert_eq!(result, PrepareResultKind::Unchanged);
        assert_eq!(fs::read(candidate).unwrap(), b"previous candidate");
    }

    #[test]
    fn changed_append_replaces_the_active_slot() {
        let state = PublicationState::initialize(data_base_url(), contents(1, 1)).unwrap();
        let ExistingPreparePlan::Candidate {
            metadata,
            upload_immutable,
        } = plan_existing_prepare(state, PrepareAction::Append, contents(2, 2), 100, 50).unwrap()
        else {
            panic!("changed append must produce a candidate");
        };
        assert!(upload_immutable);
        assert_eq!(metadata.master.slots().len(), 2);
        assert_eq!(metadata.master.slots()[0].retain_until, Some(150));
        assert_eq!(metadata.master.active_slot().serial, 2);
    }

    #[test]
    fn expired_retention_produces_a_prune_only_candidate() {
        let mut state = PublicationState::initialize(data_base_url(), contents(1, 1)).unwrap();
        state
            .publish(
                PublicationMode::AppendToActive { retain_until: 50 },
                contents(2, 2),
            )
            .unwrap();
        let active = state.slots().last().unwrap().contents.clone();
        let ExistingPreparePlan::Candidate {
            metadata,
            upload_immutable,
        } = plan_existing_prepare(state, PrepareAction::Append, active, 50, 25).unwrap()
        else {
            panic!("expired retention must produce a candidate");
        };
        assert!(!upload_immutable);
        assert_eq!(metadata.master.slots().len(), 1);
        assert_eq!(metadata.master.active_slot().serial, 2);
        assert_eq!(metadata.master.active_slot().retain_until, None);
    }

    #[test]
    fn explicit_slot_actions_never_become_noops() {
        for action in [PrepareAction::AddSlot, PrepareAction::Rotate] {
            let state = PublicationState::initialize(data_base_url(), contents(1, 1)).unwrap();
            let ExistingPreparePlan::Candidate {
                upload_immutable, ..
            } = plan_existing_prepare(state, action, contents(1, 1), 100, 50).unwrap()
            else {
                panic!("explicit slot action must produce a candidate");
            };
            assert!(upload_immutable);
        }
    }

    #[test]
    fn changed_candidate_atomically_replaces_the_existing_file() {
        let state = PublicationState::initialize(data_base_url(), contents(1, 1)).unwrap();
        let metadata = state.metadata().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let candidate = temporary.path().join("candidate.cbor");
        fs::write(&candidate, b"previous candidate").unwrap();

        let result = persist_prepare_candidate(&candidate, Some(&metadata)).unwrap();

        assert_eq!(result, PrepareResultKind::Candidate);
        assert_eq!(
            fs::read(candidate).unwrap(),
            metadata.master.encode_payload()
        );
    }

    #[tokio::test]
    async fn administrative_status_distinguishes_empty_and_missing() {
        for (presence, expected) in [
            (BucketPresence::Present, "empty"),
            (BucketPresence::Missing, "missing"),
        ] {
            let repository = FakeStatusRepository {
                presence,
                masters: Mutex::new(VecDeque::from([None])),
                stored: Vec::new(),
                transports: BTreeMap::new(),
                list_calls: std::sync::atomic::AtomicUsize::new(0),
            };
            let cache = tempfile::tempdir().unwrap();
            let report = administrative_status_from(
                &master_url(),
                trusted_keys(),
                cache.path(),
                &repository,
                0,
            )
            .await
            .unwrap();
            let value = serde_json::to_value(report).unwrap();
            assert_eq!(value["state"], expected);
            assert_eq!(value["current_slots"], 0);
            assert!(value["active_slot"].is_null());
        }
    }

    #[tokio::test]
    async fn administrative_status_retries_one_master_change() {
        let first = repository_version(1);
        let second = repository_version(2);
        let repository = FakeStatusRepository {
            presence: BucketPresence::Present,
            masters: Mutex::new(VecDeque::from([
                Some(first.master.clone()),
                Some(second.master.clone()),
                Some(second.master.clone()),
                Some(second.master.clone()),
            ])),
            stored: second.stored.clone(),
            transports: BTreeMap::from([
                (first.data_base_url.to_string(), first.transport),
                (second.data_base_url.to_string(), second.transport),
            ]),
            list_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let cache = tempfile::tempdir().unwrap();
        let report =
            administrative_status_from(&master_url(), trusted_keys(), cache.path(), &repository, 0)
                .await
                .unwrap();
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["active_slot"]["serial"], 2);
        assert_eq!(
            repository
                .list_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    #[tokio::test]
    async fn administrative_status_rejects_repeated_master_changes() {
        let first = repository_version(1);
        let second = repository_version(2);
        let third = repository_version(3);
        let repository = FakeStatusRepository {
            presence: BucketPresence::Present,
            masters: Mutex::new(VecDeque::from([
                Some(first.master.clone()),
                Some(second.master.clone()),
                Some(second.master.clone()),
                Some(third.master.clone()),
            ])),
            stored: second.stored.clone(),
            transports: BTreeMap::from([
                (first.data_base_url.to_string(), first.transport),
                (second.data_base_url.to_string(), second.transport),
            ]),
            list_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let cache = tempfile::tempdir().unwrap();
        let error =
            administrative_status_from(&master_url(), trusted_keys(), cache.path(), &repository, 0)
                .await
                .unwrap_err();
        assert!(error.to_string().contains("changed repeatedly"));
    }

    #[test]
    fn prepare_actions_are_mutually_exclusive() {
        let values = [
            "--store",
            "/tmp/store",
            "--repository",
            "s3://bucket",
            "--master-url",
            "https://example/master",
            "--output",
            "/tmp/candidate",
            "--append",
            "--rotate",
        ];
        let mut args = Arguments::new(values.into_iter().map(OsString::from));
        assert!(PrepareArgs::parse(&mut args).is_err());
    }

    #[test]
    fn every_command_accepts_the_repository_ca_bundle() {
        let path = PathBuf::from("/tmp/repository-ca.pem");

        let mut args = Arguments::new(
            [
                "--repository",
                "s3://bucket",
                "--ca-bundle",
                "/tmp/repository-ca.pem",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(
            InitArgs::parse(&mut args).unwrap().ca_bundle,
            Some(path.clone())
        );

        let mut args = Arguments::new(
            [
                "--store",
                "/tmp/store",
                "--repository",
                "s3://bucket",
                "--master-url",
                "https://example/master",
                "--ca-bundle",
                "/tmp/repository-ca.pem",
                "--output",
                "/tmp/candidate",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(
            PrepareArgs::parse(&mut args).unwrap().ca_bundle,
            Some(path.clone())
        );

        let mut args = Arguments::new(
            [
                "--candidate",
                "/tmp/candidate",
                "--repository",
                "s3://bucket",
                "--signing-key",
                "/tmp/signing-key",
                "--ca-bundle",
                "/tmp/repository-ca.pem",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(
            PublishArgs::parse(&mut args).unwrap().ca_bundle,
            Some(path.clone())
        );

        let mut args = Arguments::new(
            [
                "--master-url",
                "https://example/master",
                "--ca-bundle",
                "/tmp/repository-ca.pem",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(
            StatusArgs::parse(&mut args).unwrap().ca_bundle,
            Some(path.clone())
        );

        let mut args = Arguments::new(
            [
                "--repository",
                "s3://bucket",
                "--master-url",
                "https://example/master",
                "--ca-bundle",
                "/tmp/repository-ca.pem",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(GcArgs::parse(&mut args).unwrap().ca_bundle, Some(path));
    }

    #[test]
    fn recognizes_immutable_repository_namespaces() {
        assert!(recognized_immutable(&format!("o/{}", "a".repeat(64))));
        assert!(recognized_immutable(&format!("o/{}", "A".repeat(64))));
        assert!(!recognized_immutable("other/value"));
    }

    #[test]
    fn comparison_names_added_retained_and_modified_slots() {
        let old_slot = slot(1, None, 1);
        let current = Master::new(
            None,
            Url::parse("https://example.test/data/").unwrap(),
            vec![old_slot.clone()],
        )
        .unwrap();
        let mut retained = old_slot;
        retained.retain_until = Some(100);
        let mut modified = retained.clone();
        modified.build = BuildIndexHash::from_bytes([9; 32]);
        let candidate = Master::new(
            Some(MasterHash::from_bytes([8; 32])),
            Url::parse("https://example.test/data/").unwrap(),
            vec![modified, slot(2, None, 2)],
        )
        .unwrap();
        let comparison = publication_comparison(Some(&current), &candidate);
        assert_eq!(comparison["added_slots"], json!([2]));
        assert_eq!(
            comparison["newly_retained_slots"],
            json!([{"serial": 1, "retain_until": 100}])
        );
        assert_eq!(comparison["modified_existing_slots"], json!([1]));
        assert_eq!(comparison["pruned_slots"], json!([]));
    }

    fn data_base_url() -> Url {
        Url::parse("https://example.test/data/").unwrap()
    }

    fn object(byte: u8) -> ObjectHash {
        ObjectHash::from_bytes([byte; 32])
    }

    fn contents(key: u8, candidate: u8) -> SlotContents {
        SlotContents::new(
            [(BuildKey::from_bytes([key; 32]), object(candidate))],
            [],
            [object(candidate)],
            [],
        )
        .unwrap()
    }

    fn slot(serial: u64, retain_until: Option<u64>, byte: u8) -> Slot {
        Slot {
            serial,
            build: BuildIndexHash::from_bytes([byte; 32]),
            reuse: ReuseIndexHash::from_bytes([byte.wrapping_add(10); 32]),
            object_list: ObjectListHash::from_bytes([byte.wrapping_add(20); 32]),
            file_list: FsFileListHash::from_bytes([byte.wrapping_add(30); 32]),
            retain_until,
        }
    }

    fn master_url() -> Url {
        Url::parse("https://example.test/master").unwrap()
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42; 32])
    }

    fn trusted_keys() -> TrustedKeys {
        let key = signing_key().verifying_key();
        let key_id: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let mut trusted = TrustedKeys::default();
        trusted.insert(key_id.to_vec(), key).unwrap();
        trusted
    }

    fn repository_version(serial: u64) -> RepositoryVersion {
        let empty = Vec::new();
        let slot = Slot {
            serial,
            build: BuildIndexHash::digest(&empty),
            reuse: ReuseIndexHash::digest(&empty),
            object_list: ObjectListHash::digest(&empty),
            file_list: FsFileListHash::digest(&empty),
            retain_until: None,
        };
        let data_base_url = Url::parse(&format!("https://example.test/data-{serial}/")).unwrap();
        let master = Master::new(None, data_base_url.clone(), vec![slot.clone()]).unwrap();
        let signing_key = signing_key();
        let key_id: [u8; 32] = Sha256::digest(signing_key.verifying_key().as_bytes()).into();
        let signed = master.sign(&key_id, &signing_key).unwrap();
        let transport = MemoryTransport::default();
        transport.insert(
            master_url(),
            signed.clone(),
            "application/cose; cose-type=\"cose-sign1\"",
            "no-cache",
        );
        for (namespace, hash) in [
            ("b", slot.build.to_string()),
            ("r", slot.reuse.to_string()),
            ("lo", slot.object_list.to_string()),
            ("lf", slot.file_list.to_string()),
        ] {
            transport.insert(
                data_base_url.join(&format!("{namespace}/{hash}")).unwrap(),
                empty.clone(),
                OCTET_STREAM,
                IMMUTABLE_CACHE_CONTROL,
            );
        }
        let stored = vec![
            StoredKey {
                key: "master".to_owned(),
                size: signed.len() as u64,
            },
            StoredKey {
                key: format!("b/{}", slot.build),
                size: 0,
            },
            StoredKey {
                key: format!("r/{}", slot.reuse),
                size: 0,
            },
            StoredKey {
                key: format!("lo/{}", slot.object_list),
                size: 0,
            },
            StoredKey {
                key: format!("lf/{}", slot.file_list),
                size: 0,
            },
        ];
        RepositoryVersion {
            master: S3Object {
                bytes: signed,
                etag: format!("\"master-{serial}\""),
            },
            data_base_url,
            transport,
            stored,
        }
    }
}
