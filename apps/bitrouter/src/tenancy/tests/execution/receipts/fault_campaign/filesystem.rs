//! Opt-in actual filesystem exhaustion. Never creates or mounts a disk image.
use super::{
    fixture::{Harness, Protocol, Release, completed, identity},
    *,
};
use crate::tenancy::{
    ownership,
    receipts::capacity::{ADMISSION_FLOOR, RESERVATION, free_space},
    transaction::{
        Disposition, Phase,
        receipt_observation::{Key, SqlOutcome},
    },
};
use bitrouter_ai::tenancy::{
    identity::{InstallationId, TenantId},
    operation::{DispatchState, Error as TenantError, ErrorCode},
};
use rustix::fs::{AtFlags, Mode, OFlags, fstatfs, fstatvfs, mkdirat, openat, unlinkat};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    time::Instant,
};

const ROOT_ENV: &str = "BITROUTER_TEST_RECEIPT035_SCRATCH";
const ACK_ENV: &str = "BITROUTER_TEST_RECEIPT035_SCRATCH_ACK";
const CHILD_ENV: &str = "BITROUTER_TEST_RECEIPT035_FILESYSTEM_CHILD";
const CHILD: &str =
    "tenancy::tests::execution::receipts::fault_campaign::filesystem::filesystem_owner_child";
const SIGNAL: &str = "receipt035-filesystem-retained:";
const MARKER: &str = ".bitrouter-receipt035-scratch";
const FILLER: &str = "receipt035-fill.bin";
const DATABASE: &str = "receipt035.sqlite";
const MAX_VOLUME: u64 = 2 * 1024 * 1024 * 1024;
const BELOW_FLOOR: u64 = 640 * 1024 * 1024;
const LOG_LIMIT: u64 = 64 * 1024;
const CASE_FILES: [&str; 5] = [
    FILLER,
    DATABASE,
    "receipt035.sqlite-wal",
    "receipt035.sqlite-shm",
    ".receipt035.sqlite.tenancy-owner.lock",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledgement {
    version: u32,
    nonce: String,
    root: PathBuf,
    device: u64,
    root_inode: u64,
    total_bytes: u64,
    max_fill_bytes: u64,
    filesystem: String,
    image_path: PathBuf,
    image_device: u64,
    image_inode: u64,
    image_bytes: u64,
    dedicated_capped_image: bool,
    permit_receipt035_fill_once: bool,
}

struct Scratch {
    ack: Acknowledgement,
    root: File,
    case: File,
    case_path: PathBuf,
    claim_name: String,
    case_name: String,
    case_inode: u64,
}

fn open_directory(path: &Path) -> Result<File> {
    Ok(File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

fn regular(file: &File, device: u64, uid: u32) -> Result<std::fs::Metadata> {
    let stat = file.metadata()?;
    ensure!(
        stat.is_file() && stat.dev() == device && stat.uid() == uid && stat.nlink() == 1,
        "unowned, linked or wrong-device regular file"
    );
    Ok(stat)
}

fn bounded_text(file: File, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "setup document exceeds bound");
    Ok(bytes)
}

fn filesystem_name(directory: &File) -> Result<String> {
    let stat = fstatfs(directory)?;
    #[cfg(target_os = "macos")]
    {
        let bytes: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Ok(String::from_utf8(bytes)?)
    }
    #[cfg(target_os = "linux")]
    {
        // ext2/3/4 share this magic. Parent's acknowledged image must identify
        // ext4; the report still records the observed shared magic explicitly.
        // https://man7.org/linux/man-pages/man2/statfs.2.html
        ensure!(
            stat.f_type as u64 == 0xef53,
            "unsupported Linux scratch filesystem"
        );
        Ok("ext-family-0xef53".into())
    }
}

fn available(directory: &File) -> Result<(u64, u64)> {
    let stat = fstatvfs(directory)?;
    Ok((
        stat.f_blocks
            .checked_mul(stat.f_frsize)
            .context("volume size overflow")?,
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .context("free size overflow")?,
    ))
}

impl Scratch {
    fn open(child: bool) -> Result<Self> {
        let uid = rustix::process::geteuid().as_raw();
        ensure!(uid != 0, "scratch campaign requires non-root user");
        let supplied =
            PathBuf::from(std::env::var_os(ROOT_ENV).context("explicit scratch mount required")?);
        let ack_path = PathBuf::from(
            std::env::var_os(ACK_ENV).context("explicit scratch acknowledgement required")?,
        );
        ensure!(supplied.is_absolute() && ack_path.is_absolute());
        ensure!(supplied.as_os_str().len() <= 2048 && ack_path.as_os_str().len() <= 2048);
        let root_path = supplied.canonicalize()?;
        ensure!(
            root_path == supplied && root_path != Path::new("/"),
            "scratch must be exact canonical mount root"
        );
        let cwd = std::env::current_dir()?.canonicalize()?;
        ensure!(
            !cwd.starts_with(&root_path),
            "scratch contains the running workspace"
        );
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home).canonicalize()?;
            ensure!(
                !home.starts_with(&root_path),
                "scratch contains the home directory"
            );
        }
        let root = open_directory(&root_path)?;
        let root_meta = root.metadata()?;
        ensure!(
            root_meta.uid() == uid,
            "scratch root must be owned by invoking user"
        );
        ensure!(
            std::fs::metadata(root_path.parent().context("mount parent")?)?.dev()
                != root_meta.dev(),
            "not a separate mounted filesystem"
        );
        ensure!(
            ack_path.canonicalize()? == ack_path,
            "acknowledgement path must be canonical without symlinks"
        );
        ensure!(
            !ack_path.starts_with(&root_path),
            "acknowledgement must survive exhausted mount"
        );
        let ack_file = File::from(rustix::fs::open(
            &ack_path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let ack_meta = ack_file.metadata()?;
        ensure!(
            ack_meta.is_file()
                && ack_meta.uid() == uid
                && ack_meta.nlink() == 1
                && ack_meta.mode() & 0o777 == 0o600
        );
        let ack: Acknowledgement = serde_json::from_slice(&bounded_text(ack_file, 4096)?)?;
        ensure!(ack.version == 1 && ack.dedicated_capped_image && ack.permit_receipt035_fill_once);
        ensure!(
            ack.nonce.len() == 32
                && ack
                    .nonce
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        ensure!(
            ack.root == root_path
                && ack.device == root_meta.dev()
                && ack.root_inode == root_meta.ino()
        );
        let (total, free) = available(&root)?;
        ensure!(total == ack.total_bytes && total <= MAX_VOLUME && total > ADMISSION_FLOOR);
        ensure!(ack.max_fill_bytes > ADMISSION_FLOOR && ack.max_fill_bytes <= total);
        if !child {
            ensure!(
                free > ADMISSION_FLOOR + 64 * 1024 * 1024,
                "insufficient initial headroom"
            );
        }
        ensure!(
            filesystem_name(&root)? == ack.filesystem,
            "filesystem identity differs from acknowledgement"
        );
        #[cfg(target_os = "macos")]
        ensure!(
            ack.filesystem == "apfs",
            "this macOS campaign requires the acknowledged APFS image"
        );
        ensure!(
            ack.image_path.is_absolute()
                && ack.image_path.canonicalize()? == ack.image_path
                && !ack.image_path.starts_with(&root_path)
        );
        let image = std::fs::symlink_metadata(&ack.image_path)?;
        ensure!(image.is_file() && image.uid() == uid && image.nlink() == 1);
        ensure!(
            image.dev() != ack.device
                && image.dev() == ack.image_device
                && image.ino() == ack.image_inode
        );
        ensure!(
            image.len() == ack.image_bytes && image.len() <= MAX_VOLUME && image.len() >= total
        );
        // Refuse sparse backing images. Parent separately verifies this mounted
        // device is backed by this dedicated image; st_dev alone cannot do so.
        // https://support.apple.com/guide/disk-utility/dskutl11888/mac
        ensure!(
            image
                .blocks()
                .checked_mul(512)
                .context("image allocation overflow")?
                >= image.len(),
            "backing image is not fully allocated"
        );
        let marker = File::from(openat(
            &root,
            MARKER,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let marker_meta = regular(&marker, ack.device, uid)?;
        ensure!(marker_meta.mode() & 0o777 == 0o600);
        ensure!(
            bounded_text(marker, 64)? == format!("{}\n", ack.nonce).as_bytes(),
            "mount marker mismatch"
        );
        let claim_name = format!(".receipt035-{}.claim", ack.nonce);
        let case_name = format!("receipt035-{}", ack.nonce);
        for entry in std::fs::read_dir(&root_path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("unexpected mount filename")?;
            let metadata = entry.file_type()?;
            let platform_metadata = cfg!(target_os = "macos")
                && match name {
                    ".Trashes" | ".fseventsd" | ".Spotlight-V100" => metadata.is_dir(),
                    ".metadata_never_index" => metadata.is_file(),
                    _ => false,
                };
            ensure!(
                name == MARKER
                    || platform_metadata
                    || (child && (name == claim_name || name == case_name)),
                "scratch is not fresh: {name}"
            );
        }
        let mut claim_created = false;
        let mut case_created = false;
        let case_path = root_path.join(&case_name);
        let setup: Result<(File, u64)> = (|| {
            if !child {
                let mut claim = File::from(openat(
                    &root,
                    claim_name.as_str(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )?);
                claim_created = true;
                regular(&claim, ack.device, uid)?;
                claim.write_all(format!("{}\n", ack.nonce).as_bytes())?;
                claim.sync_all()?;
                mkdirat(
                    &root,
                    case_name.as_str(),
                    Mode::RUSR | Mode::WUSR | Mode::XUSR,
                )?;
                case_created = true;
            } else {
                let claim = File::from(openat(
                    &root,
                    claim_name.as_str(),
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                regular(&claim, ack.device, uid)?;
                ensure!(bounded_text(claim, 64)? == format!("{}\n", ack.nonce).as_bytes());
            }
            let case = File::from(openat(
                &root,
                case_name.as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            let metadata = case.metadata()?;
            ensure!(
                metadata.dev() == ack.device
                    && metadata.uid() == uid
                    && metadata.mode() & 0o777 == 0o700
            );
            ensure!(
                std::fs::read_dir(&case_path)?.next().is_none(),
                "case directory must initially be empty"
            );
            Ok((case, metadata.ino()))
        })();
        let (case, case_inode) = match setup {
            Ok(value) => value,
            Err(error) => {
                // No child exists yet. Remove only names this invocation
                // successfully created; an EXCL failure authorizes no deletion.
                if case_created {
                    unlinkat(&root, case_name.as_str(), AtFlags::REMOVEDIR)
                        .with_context(|| format!("setup failed ({error:#}); empty case cleanup unconfirmed: {case_path:?}"))?;
                }
                if claim_created {
                    remove_regular(&root, &claim_name, ack.device).with_context(|| {
                        format!("setup failed ({error:#}); claim cleanup unconfirmed")
                    })?;
                }
                return Err(error);
            }
        };
        Ok(Self {
            ack,
            root,
            case,
            case_path,
            claim_name,
            case_name,
            case_inode,
        })
    }

    fn verify(&self) -> Result<()> {
        let root = std::fs::symlink_metadata(&self.ack.root)?;
        let case = std::fs::symlink_metadata(&self.case_path)?;
        ensure!(
            root.is_dir() && root.dev() == self.ack.device && root.ino() == self.ack.root_inode
        );
        ensure!(case.is_dir() && case.dev() == self.ack.device && case.ino() == self.case_inode);
        ensure!(available(&self.root)?.0 == self.ack.total_bytes);
        Ok(())
    }

    fn database(&self) -> PathBuf {
        self.case_path.join(DATABASE)
    }

    fn remove_filler(&self) -> Result<()> {
        self.verify()?;
        remove_regular(&self.case, FILLER, self.ack.device)
    }

    fn cleanup(&self) -> Result<()> {
        self.verify()?;
        // Only after child exit and the final pool/Lease have been dropped.
        // Unknown files cause refusal; there is no recursive delete.
        for entry in std::fs::read_dir(&self.case_path)? {
            let entry = entry?;
            ensure!(
                CASE_FILES.iter().any(|name| entry.file_name() == *name),
                "unexpected case file retained"
            );
        }
        for name in CASE_FILES {
            remove_regular(&self.case, name, self.ack.device)?;
        }
        unlinkat(&self.root, self.case_name.as_str(), AtFlags::REMOVEDIR)?;
        remove_regular(&self.root, &self.claim_name, self.ack.device)?;
        ensure!(!self.case_path.exists());
        Ok(())
    }
}

fn remove_regular(directory: &File, name: &str, device: u64) -> Result<()> {
    let fd = match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let file = File::from(fd);
    regular(&file, device, rustix::process::geteuid().as_raw())?;
    unlinkat(directory, name, AtFlags::empty())?;
    Ok(())
}

struct Filler {
    file: File,
    directory: File,
    device: u64,
    cap: u64,
    written: u64,
}
#[derive(Debug, Serialize, Deserialize)]
struct Full {
    operation: String,
    errno: i32,
    written: u64,
    available: u64,
}

impl Filler {
    fn new(scratch: &Scratch) -> Result<Self> {
        scratch.verify()?;
        let file = File::from(openat(
            &scratch.case,
            FILLER,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        regular(
            &file,
            scratch.ack.device,
            rustix::process::geteuid().as_raw(),
        )?;
        Ok(Self {
            file,
            directory: scratch.case.try_clone()?,
            device: scratch.ack.device,
            cap: scratch.ack.max_fill_bytes,
            written: 0,
        })
    }

    fn fill(mut self, target_free: Option<u64>, seconds: u64) -> Result<(Self, Option<Full>)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        // Ordinary finite writes, no sparse allocation. Existing rand dependency
        // supplies one reusable incompressible block; it is not a secret.
        let block: [u8; 64 * 1024] = std::array::from_fn(|_| rand::random());
        loop {
            ensure!(
                std::time::Instant::now() < deadline,
                "bounded fill deadline"
            );
            ensure!(
                self.file.metadata()?.dev() == self.device
                    && self.directory.metadata()?.dev() == self.device
            );
            let free = available(&self.directory)?.1;
            if target_free.is_some_and(|target| free <= target) {
                self.file.sync_all()?;
                return Ok((self, None));
            }
            let remaining = self
                .cap
                .checked_sub(self.written)
                .context("fill accounting overflow")?;
            ensure!(remaining > 0, "finite fill cap reached without ENOSPC");
            let length = usize::try_from(remaining.min(block.len() as u64))?;
            // Write and sync errors are classified separately; only actual28
            // qualifies, never EIO, permissions, a timeout or a mocked value.
            match self.file.write(&block[..length]) {
                Ok(0) => anyhow::bail!("zero-progress filler write"),
                Ok(bytes) => {
                    self.written = self
                        .written
                        .checked_add(bytes as u64)
                        .context("fill byte overflow")?
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(28) && target_free.is_none() => {
                    let full = Full {
                        operation: "write".into(),
                        errno: 28,
                        written: self.written,
                        available: available(&self.directory)?.1,
                    };
                    // Also request synchronization of bytes already accepted;
                    // a second ENOSPC is recorded by the original write witness.
                    match self.file.sync_all() {
                        Ok(()) => {}
                        Err(error) if error.raw_os_error() == Some(28) => {}
                        Err(error) => return Err(error.into()),
                    }
                    return Ok((self, Some(full)));
                }
                Err(error) => return Err(error.into()),
            }
            if self.written.is_multiple_of(4 * 1024 * 1024) {
                match self.file.sync_all() {
                    Ok(()) => {}
                    Err(error) if error.raw_os_error() == Some(28) && target_free.is_none() => {
                        let full = Full {
                            operation: "sync_all".into(),
                            errno: 28,
                            written: self.written,
                            available: available(&self.directory)?.1,
                        };
                        return Ok((self, Some(full)));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Saved {
    execution: String,
    receipt: String,
    disposition: String,
    digest: String,
}

async fn saved(
    db: &sea_orm::DatabaseConnection,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Option<Saved>> {
    let row = db.query_one(Statement::from_sql_and_values(
        db.get_database_backend(),
        "SELECT receipt_json, disposition_json, content_digest FROM tenancy_execution_receipts WHERE installation_id = ? AND tenant_id = ? AND execution_id = ?",
        [installation.as_str().into(), tenant.as_str().into(), id.as_str().into()],
    )).await?;
    row.map(|row| {
        Ok(Saved {
            execution: id.as_str().into(),
            receipt: row.try_get("", "receipt_json")?,
            disposition: row.try_get("", "disposition_json")?,
            digest: row.try_get("", "content_digest")?,
        })
    })
    .transpose()
}

async fn bounded_body(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= WIRE_LIMIT.saturating_sub(bytes.len()),
            "body exceeded bound"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Default)]
struct Frames {
    prefix: bool,
    error: bool,
    done: bool,
    successful_done: bool,
}
fn frames(bytes: &[u8]) -> Result<Frames> {
    let text = std::str::from_utf8(bytes)?;
    let mut result = Frames::default();
    // https://platform.openai.com/docs/api-reference/chat-streaming
    // Chat terminal usage/finish_reason remain provisional. Only a DONE before
    // any error is success; a later error must never erase that observation.
    for frame in text
        .split_inclusive("\n\n")
        .filter(|frame| frame.ends_with("\n\n"))
    {
        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if data == "[DONE]" {
                result.done = true;
                result.successful_done |= !result.error;
                continue;
            }
            let value: Value = serde_json::from_str(data)?;
            result.error |= value.get("error").is_some_and(|v| !v.is_null())
                || value.get("type").and_then(Value::as_str) == Some("error");
            result.prefix |= value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty());
        }
    }
    Ok(result)
}

async fn stream_observation(
    request: reqwest::RequestBuilder,
    prefix: tokio::sync::oneshot::Sender<ExecutionId>,
) -> Result<ExecutionId> {
    let mut response = request.send().await?;
    ensure!(
        response.status().is_success(),
        "stream refused before admitted work"
    );
    let id = identity(&response, false)?;
    let mut sender = Some(prefix);
    let mut seen = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(chunk.len() <= WIRE_LIMIT.saturating_sub(seen.len()));
        seen.extend_from_slice(&chunk);
        let observed = frames(&seen)?;
        ensure!(
            !observed.successful_done,
            "success DONE escaped before failed receipt durability"
        );
        if observed.prefix
            && let Some(sender) = sender.take()
        {
            sender
                .send(id.clone())
                .map_err(|_| anyhow::anyhow!("prefix observer closed"))?;
        }
    }
    let observed = frames(&seen)?;
    ensure!(
        observed.prefix && observed.error && !observed.successful_done,
        "missing actual failed stream framing"
    );
    Ok(id)
}

fn capacity(harness: &Harness) -> Result<(u64, u64, bool)> {
    harness
        .fixture
        .service
        .admission
        .receipt_capacity_for_test()
        .map_err(|code| anyhow::anyhow!("capacity snapshot {code:?}"))
}

fn retained(harness: &Harness) -> Result<()> {
    ensure!(
        capacity(harness)?.0 == RESERVATION,
        "completion bytes were released or duplicated"
    );
    let counts = harness
        .fixture
        .service
        .admission
        .ingress_for_test()
        .map_err(|code| anyhow::anyhow!("admission snapshot {code:?}"))?;
    ensure!(
        counts == (0, 0, 1, 0),
        "unexpected ingress/active/queue counts {counts:?}"
    );
    ensure!(
        harness
            .fixture
            .service
            .admission
            .wait_drained()
            .now_or_never()
            .is_none()
    );
    Ok(())
}

async fn pragma(
    db: &sea_orm::DatabaseConnection,
    sql: &'static str,
    column: &'static str,
) -> Result<i64> {
    Ok(db
        .query_one(Statement::from_string(db.get_database_backend(), sql))
        .await?
        .context("pragma row")?
        .try_get("", column)?)
}

fn wal_metadata(path: &Path) -> Result<(u64, u64)> {
    let path = path.with_file_name("receipt035.sqlite-wal");
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_file());
            Ok((metadata.len(), metadata.blocks()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((0, 0)),
        Err(error) => Err(error.into()),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    nonce: String,
    installation: String,
    tenant: String,
    earlier: Saved,
    execution: String,
    attempt: String,
    candidate_digest: String,
    candidate_bytes: u64,
    sqlite_code: i32,
    full: Full,
    filesystem: String,
    device: u64,
    total_bytes: u64,
    sqlite_version: String,
    synchronous: i64,
    page_size: i64,
    wal_autocheckpoint: i64,
    max_page_count: i64,
    wal_at_gate: (u64, u64),
    wal_after_failure: (u64, u64),
}

async fn exercise(scratch: &Scratch) -> Result<Evidence> {
    scratch.verify()?;
    let database = scratch.database();
    let mut harness = Harness::new(Protocol::Chat, Some(&database)).await?;
    ensure!(harness.fixture.service.receipt_volume.as_deref() == Some(scratch.case_path.as_path()));
    ensure!(free_space(Some(&scratch.case_path))? > ADMISSION_FLOOR);
    let mode = harness
        .fixture
        .db
        .query_one(Statement::from_string(
            harness.fixture.db.get_database_backend(),
            "PRAGMA journal_mode=WAL",
        ))
        .await?
        .context("journal mode")?;
    ensure!(
        mode.try_get::<String>("", "journal_mode")?
            .eq_ignore_ascii_case("wal")
    );
    // Actual connection settings are evidence, not assumed production defaults.
    // https://www.sqlite.org/pragma.html#pragma_wal_checkpoint
    let synchronous = pragma(&harness.fixture.db, "PRAGMA synchronous", "synchronous").await?;
    let page_size = pragma(&harness.fixture.db, "PRAGMA page_size", "page_size").await?;
    let wal_autocheckpoint = pragma(
        &harness.fixture.db,
        "PRAGMA wal_autocheckpoint",
        "wal_autocheckpoint",
    )
    .await?;
    let max_page_count = pragma(
        &harness.fixture.db,
        "PRAGMA max_page_count",
        "max_page_count",
    )
    .await?;
    ensure!(
        u64::try_from(max_page_count)?
            .checked_mul(u64::try_from(page_size)?)
            .context("SQLite maximum bytes")?
            > MAX_VOLUME,
        "SQLite page limit could fake volume exhaustion"
    );
    let sqlite_version: String = harness
        .fixture
        .db
        .query_one(Statement::from_string(
            harness.fixture.db.get_database_backend(),
            "SELECT sqlite_version() AS version",
        ))
        .await?
        .context("SQLite version")?
        .try_get("", "version")?;
    // Only the client timeout is widened for finite real disk I/O; H's receipt
    // deadline and the fixed execution/admission limits remain untouched.
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .timeout(Duration::from_secs(90))
        .build()?;
    let request = harness.request(&http, false);
    let control = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut response = request.send().await?;
        ensure!(response.status().is_success());
        let id = identity(&response, false)?;
        let bytes = bounded_body(&mut response).await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure!(value.get("error").is_none());
        Ok::<_, anyhow::Error>(id)
    }));
    harness.provider.accepted().await?;
    harness.provider.open.cancel();
    let earlier_id = tokio::time::timeout(WAIT, control).await???;
    tokio::time::timeout(WAIT, harness.fixture.service.admission.wait_drained()).await?;
    ensure!(capacity(&harness)?.0 == 0);
    let control_receipt = harness.receipt(&earlier_id).await?;
    completed(&control_receipt)?;
    ensure!(control_receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed);
    drop(control_receipt);
    let earlier = saved(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &earlier_id,
    )
    .await?
    .context("positive immutable WAL receipt")?;
    ensure!(
        earlier.receipt.len() + earlier.disposition.len() < 16 * 1024 && earlier.digest.len() == 64
    );
    ensure!(
        harness.count("tenancy_execution_receipts").await? == 1
            && harness.provider.calls.load(Ordering::SeqCst) == 1
    );
    let checkpoint = harness
        .fixture
        .db
        .query_one(Statement::from_string(
            harness.fixture.db.get_database_backend(),
            "PRAGMA wal_checkpoint(TRUNCATE)",
        ))
        .await?
        .context("WAL checkpoint")?;
    ensure!(checkpoint.try_get::<i64>("", "busy")? == 0);
    ensure!(
        wal_metadata(&database)? == (0, 0),
        "fresh WAL allocation precondition not established"
    );

    let (prefix_tx, prefix_rx) = tokio::sync::oneshot::channel();
    let request = harness.request(&http, true);
    let streaming = AbortOnDropHandle::new(tokio::spawn(stream_observation(request, prefix_tx)));
    harness.provider.accepted().await?;
    let id = tokio::time::timeout(WAIT, prefix_rx).await??;
    ensure!(id != earlier_id);
    harness
        .fixture
        .service
        .cleanup
        .observe_receipt_for_test(Key {
            installation: harness.fixture.installation.clone(),
            tenant: harness.fixture.tenant.clone(),
            execution: id.clone(),
        })?;
    retained(&harness)?;

    let filler = Filler::new(scratch)?;
    let (filler, premature) =
        tokio::task::spawn_blocking(move || filler.fill(Some(BELOW_FLOOR), 20)).await??;
    ensure!(premature.is_none());
    let free = free_space(Some(&scratch.case_path))?;
    ensure!(
        free > 256 * 1024 * 1024 && free < ADMISSION_FLOOR,
        "below-floor control missed usable interval"
    );
    let mut rejected = tokio::time::timeout(WAIT, harness.request(&http, false).send()).await??;
    ensure!(
        !rejected.status().is_success()
            && rejected.headers().get("x-bitrouter-request-id").is_none()
    );
    let error: TenantError =
        serde_json::from_slice(&tokio::time::timeout(WAIT, bounded_body(&mut rejected)).await??)?;
    ensure!(
        error.code == ErrorCode::CapacityExceeded
            && error.dispatch_state == DispatchState::NotStarted
            && error.execution_id.is_none()
    );
    ensure!(harness.provider.calls.load(Ordering::SeqCst) == 2);
    ensure!(harness.count("tenancy_execution_receipts").await? == 1);
    retained(&harness)?;
    let below = capacity(&harness)?;
    ensure!(
        below.1 < ADMISSION_FLOOR && !below.2,
        "below-floor refusal unexpectedly fenced receipts"
    );

    // Install after the third request's real credential read. Chat has no034
    // publication, so only this watched receipt can enter the next COMMIT.
    let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
    let release = Release(harness.fixture.service.clone());
    harness.provider.tail.cancel();
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .with_context(|| {
            format!(
                "did not reach actual COMMIT; Store includes lookup/INSERT/read-back: {:?}",
                harness.fixture.service.cleanup.receipt_trace_for_test()
            )
        })?;
    let before = harness.fixture.service.cleanup.receipt_trace_for_test()?;
    ensure!(!before.overflow && before.facts[1].is_none() && before.key.execution == id);
    let fact = before.facts[0]
        .as_ref()
        .context("actual receipt store fact before COMMIT")?;
    ensure!(
        fact.begin
            == Some(SqlOutcome {
                ok: true,
                sqlite_code: None
            })
            && fact.store
                == Some(SqlOutcome {
                    ok: true,
                    sqlite_code: None
                })
            && fact.inserted == Some(true)
    );
    ensure!(
        !fact.check_failed
            && fact.commit.is_none()
            && fact.rollback.is_none()
            && fact.fingerprint.attempt_count == 1
    );
    let fingerprint = fact.fingerprint.clone();
    let wal_at_gate = wal_metadata(&database)?;
    // A receipt already written to allocated WAL pages would not exercise the
    // intended append-on-COMMIT case. Refuse instead of passing via another fault.
    ensure!(
        wal_at_gate == (0, 0),
        "WAL spill/reuse precludes this COMMIT-target case"
    );
    let gate_start = Instant::now();
    let filled = tokio::task::spawn_blocking(move || filler.fill(None, 19));
    let (filler, full) = tokio::time::timeout(Duration::from_secs(20), filled).await???;
    ensure!(
        gate_start.elapsed() < Duration::from_secs(20),
        "gate fill exceeded20s"
    );
    let full = full.context("actual bounded ENOSPC required")?;
    ensure!(
        full.errno == 28
            && full.written <= scratch.ack.max_fill_bytes
            && filler.device == scratch.ack.device
    );
    scratch.verify()?;
    drop(release);
    eprintln!(
        "receipt035-filesystem-full:{}",
        serde_json::to_string(&full)?
    );
    let observed_id = tokio::time::timeout(WAIT, streaming)
        .await??
        .with_context(|| {
            format!(
                "downstream failure framing did not qualify; actual receipt trace: {:?}",
                harness.fixture.service.cleanup.receipt_trace_for_test()
            )
        })?;
    ensure!(observed_id == id);
    tokio::time::timeout(WAIT, harness.fixture.service.cleanup.quarantined()).await?;
    let trace = harness.fixture.service.cleanup.receipt_trace_for_test()?;
    ensure!(!trace.overflow && trace.key == before.key && trace.facts[1].is_none());
    eprintln!("receipt035-filesystem-sql:{trace:?}");
    let fact = trace.facts[0]
        .as_ref()
        .context("consuming receipt result")?;
    ensure!(fact.fingerprint == fingerprint && !fact.check_failed && fact.rollback.is_none());
    let commit = fact
        .commit
        .context("real consuming COMMIT did not return")?;
    let code = commit
        .sqlite_code
        .context("consuming error lacked typed SQLite code")?;
    // The extended code's low byte is the primary result. Do not accept generic
    // IOERR, deadline or HTTP503 as proof of SQLite disk-full.
    // https://www.sqlite.org/rescode.html#full
    ensure!(
        !commit.ok && code & 0xff == 13 && fact.disposition == Disposition::Ambiguous,
        "not actual consuming SQLITE_FULL: {fact:?}"
    );
    let wal_after_failure = wal_metadata(&database)?;
    retained(&harness)?;
    ensure!(!harness.fixture.service.admission.receipts_ready());
    ensure!(
        harness.fixture.service.cleanup.ensure_clean().is_err()
            && harness.fixture.service.current().is_err()
    );

    drop(filler);
    scratch.remove_filler()?;
    tokio::time::timeout(WAIT, async {
        loop {
            if free_space(Some(&scratch.case_path))? > ADMISSION_FLOOR {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    retained(&harness)?;
    ensure!(
        !harness.fixture.service.admission.receipts_ready(),
        "freeing space silently unfenced receipts"
    );
    // One actual shutdown future, driven to its explicit failure. A timeout or
    // cancellation is not clean shutdown or an accepted custody observation.
    {
        let shutdown = harness.fixture.service.shutdown();
        tokio::pin!(shutdown);
        ensure!(tokio::time::timeout(WAIT, &mut shutdown).await?.is_err());
    }
    retained(&harness)?;
    ensure!(
        harness.provider.calls.load(Ordering::SeqCst) == 2,
        "receipt failure repeated inference"
    );
    let evidence = Evidence {
        nonce: scratch.ack.nonce.clone(),
        installation: harness.fixture.installation.as_str().into(),
        tenant: harness.fixture.tenant.as_str().into(),
        earlier,
        execution: id.as_str().into(),
        attempt: fingerprint
            .first_attempt
            .as_ref()
            .context("actual attempt id")?
            .as_str()
            .into(),
        candidate_digest: fingerprint.digest,
        candidate_bytes: fingerprint.bytes,
        sqlite_code: code,
        full,
        filesystem: scratch.ack.filesystem.clone(),
        device: scratch.ack.device,
        total_bytes: scratch.ack.total_bytes,
        sqlite_version,
        synchronous,
        page_size,
        wal_autocheckpoint,
        max_page_count,
        wal_at_gate,
        wal_after_failure,
    };
    let db = harness.fixture.db.clone();
    let proof = harness.fixture.service.cleanup.clone();
    harness.close().await?;
    tokio::time::timeout(WAIT, db.close()).await??;
    ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
    // None of these diagnostic values own the actor or Lease. The child signal
    // is emitted only after ordinary handles and the observer Proof are gone.
    drop((proof, http, gate, before, trace));
    Ok(evidence)
}

#[tokio::test]
#[ignore = "only the bounded filesystem parent invokes this with an acknowledged scratch image"]
async fn filesystem_owner_child() -> Result<()> {
    ensure!(
        std::env::var(CHILD_ENV).as_deref() == Ok("1"),
        "filesystem child requires parent launch"
    );
    let scratch = Scratch::open(true)?;
    let evidence = tokio::time::timeout(Duration::from_secs(90), exercise(&scratch)).await??;
    let line = serde_json::to_string(&evidence)?;
    ensure!(line.len() < LOG_LIMIT as usize / 2);
    // Serial libtest writes a non-newline name prefix. Keep strict whole-line
    // matching by separating the actual child signal with a leading newline.
    println!("\n{SIGNAL}{line}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(128));
    let mut command = String::new();
    tokio::time::timeout(Duration::from_secs(10), input.read_line(&mut command)).await??;
    ensure!(
        command == format!("exit {}\n", scratch.ack.nonce),
        "missing exact parent exit acknowledgement"
    );
    Ok(())
}

async fn signal(
    output: &mut (impl tokio::io::AsyncBufRead + Unpin),
    scratch: &Scratch,
) -> Result<Evidence> {
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let count = output.read_line(&mut line).await?;
        ensure!(count != 0, "child exited before retained-owner evidence");
        total = total
            .checked_add(count)
            .context("diagnostic byte overflow")?;
        ensure!(total <= LOG_LIMIT as usize);
        let line = line
            .strip_suffix('\n')
            .context("unterminated child signal line")?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(json) = line.strip_prefix(SIGNAL) {
            let evidence: Evidence = serde_json::from_str(json)?;
            ensure!(
                evidence.nonce == scratch.ack.nonce
                    && evidence.device == scratch.ack.device
                    && evidence.total_bytes == scratch.ack.total_bytes
            );
            ensure!(
                evidence.sqlite_code & 0xff == 13
                    && evidence.full.errno == 28
                    && evidence.full.written <= scratch.ack.max_fill_bytes
            );
            ensure!(evidence.filesystem == scratch.ack.filesystem);
            scratch.verify()?;
            match ownership::acquire_path(&scratch.database(), false) {
                Err(error) => ensure!(
                    matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ),
                    "unrelated file-owner probe failure: {error:#}"
                ),
                Ok(owner) => {
                    drop(owner);
                    anyhow::bail!("ambiguous COMMIT released its file owner before child exit");
                }
            }
            return Ok(evidence);
        }
    }
}

async fn reconnect(scratch: &Scratch, evidence: &Evidence) -> Result<()> {
    scratch.verify()?;
    let owner = ownership::acquire_path(&scratch.database(), false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let result: Result<()> = async {
        let installation: InstallationId = evidence.installation.as_str().try_into()?;
        let tenant: TenantId = evidence.tenant.as_str().try_into()?;
        let earlier_id: ExecutionId = evidence.earlier.execution.as_str().try_into()?;
        let id: ExecutionId = evidence.execution.as_str().try_into()?;
        let integrity: String = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "PRAGMA integrity_check",
            ))
            .await?
            .context("integrity result")?
            .try_get("", "integrity_check")?;
        ensure!(integrity == "ok");
        let earlier = saved(&db, &installation, &tenant, &earlier_id)
            .await?
            .context("earlier receipt missing after reopen")?;
        ensure!(
            earlier == evidence.earlier,
            "earlier immutable receipt changed"
        );
        completed(
            &store::lookup(&db, &installation, &tenant, &earlier_id)
                .await?
                .context("validated earlier receipt")?,
        )?;
        let row = saved(&db, &installation, &tenant, &id).await?;
        let decoded = store::lookup(&db, &installation, &tenant, &id).await?;
        let expected_count = 1 + i64::from(row.is_some());
        match (row, decoded) {
            (Some(row), Some(receipt)) => {
                ensure!(
                    row.digest == evidence.candidate_digest
                        && (row.receipt.len() + row.disposition.len()) as u64
                            == evidence.candidate_bytes
                );
                ensure!(
                    receipt.attempts.len() == 1
                        && receipt.attempts[0].attempt_id.as_str() == evidence.attempt
                );
                completed(&receipt)?;
            }
            (None, None) => {} // Absence is not a rollback or cleanup proof.
            _ => anyhow::bail!("raw and validated receipt disagree"),
        }
        let count: i64 = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM tenancy_execution_receipts",
            ))
            .await?
            .context("receipt count")?
            .try_get("", "n")?;
        ensure!(
            count == expected_count,
            "unexpected durable receipt effects"
        );
        // This is Chat, so a separate034 claim or continuation is unexpected.
        let continuations: i64 = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM provider_continuations",
            ))
            .await?
            .context("continuation count")?
            .try_get("", "n")?;
        ensure!(continuations == 0);
        Ok(())
    }
    .await;
    let closed = db.close().await;
    drop(owner);
    closed?;
    result
}

#[tokio::test]
#[ignore = "requires an explicit fresh capped scratch image and acknowledgement; never mounts or fills the host filesystem"]
async fn actual_enospc_retains_receipt_custody() -> Result<()> {
    let start = Instant::now();
    let overall = start + Duration::from_secs(120);
    let executable = std::env::current_exe()?;
    let scratch = Scratch::open(false)?;
    let command = tokio::process::Command::new(executable)
        .args([
            "--exact",
            CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(ROOT_ENV, &scratch.ack.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match command {
        Ok(child) => child,
        Err(error) => {
            scratch.cleanup()?;
            return Err(error.into());
        }
    };
    // Libtest writes its terminal status after the child acknowledges exit.
    // Keep the same bounded stdout reader alive through actual process exit;
    // dropping it after the evidence line would make that final write fail.
    let mut output = BufReader::new(
        child
            .stdout
            .take()
            .context("child stdout")?
            .take(LOG_LIMIT + 1),
    );
    let stderr = child.stderr.take().context("child stderr")?;
    let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(LOG_LIMIT + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= LOG_LIMIT as usize,
            "child stderr exceeded bound"
        );
        Ok::<_, anyhow::Error>(bytes)
    }));
    let outcome = tokio::time::timeout_at(
        start + Duration::from_secs(90),
        signal(&mut output, &scratch),
    )
    .await
    .context("filesystem experiment deadline; not a passing result")
    .and_then(|value| value);
    let mut exit_request = Ok(());
    if outcome.is_ok() {
        if let Some(mut stdin) = child.stdin.take() {
            exit_request = tokio::time::timeout(
                Duration::from_secs(2),
                stdin.write_all(format!("exit {}\n", scratch.ack.nonce).as_bytes()),
            )
            .await
            .context("child exit command deadline")
            .and_then(|value| value.map_err(Into::into));
        } else {
            exit_request = Err(anyhow::anyhow!("child stdin missing"));
        }
    } else {
        drop(child.stdin.take());
    }
    let mut forced = false;
    let reaped = match tokio::time::timeout_at(
        (Instant::now() + Duration::from_secs(5)).min(start + Duration::from_secs(95)),
        child.wait(),
    )
    .await
    {
        Ok(status) => status.map_err(anyhow::Error::from),
        Err(_) => {
            forced = true;
            match child.start_kill() {
                Ok(()) => tokio::time::timeout_at(
                    (Instant::now() + Duration::from_secs(5)).min(start + Duration::from_secs(100)),
                    child.wait(),
                )
                .await
                .context("child exit unconfirmed; retain scratch paths")
                .and_then(|value| value.map_err(Into::into)),
                Err(error) => Err(error.into()),
            }
        }
    };
    let status = match reaped {
        Ok(status) => status,
        Err(error) => {
            return Err(error.context(format!(
                "child not reaped; preserve {} and supplied image",
                scratch.case_path.display()
            )));
        }
    };
    drop(output);
    let diagnostics = tokio::time::timeout_at(
        (Instant::now() + Duration::from_secs(2)).min(overall),
        diagnostics,
    )
    .await;
    // Once the exact child is reaped, recovery can remove only its filler even
    // if it failed before the normal recovery branch. Never delete a live DB.
    scratch.remove_filler()?;
    let recovery = match &outcome {
        Ok(evidence) => tokio::time::timeout_at(
            start + Duration::from_secs(115),
            reconnect(&scratch, evidence),
        )
        .await
        .context("post-exit reconnect deadline; no cleanup claim")
        .and_then(|value| value),
        Err(_) => Ok(()),
    };
    // Real Lease reacquisition also detects an unclosed recovery pool after a
    // timeout. No zero-row observation can replace this ownership check.
    let owner = ownership::acquire_path(&scratch.database(), true).with_context(|| {
        format!(
            "post-exit owner unconfirmed; preserve {}",
            scratch.case_path.display()
        )
    })?;
    drop(owner);
    let case_path = scratch.case_path.clone();
    let cleaned = tokio::time::timeout_at(
        overall,
        tokio::task::spawn_blocking(move || scratch.cleanup()),
    )
    .await
    .context("owned-path cleanup deadline")
    .and_then(|value| value.map_err(anyhow::Error::from))
    .and_then(|value| value);
    let stderr = diagnostics
        .context("diagnostic join deadline")???
        .into_iter()
        .map(char::from)
        .collect::<String>();
    cleaned.with_context(|| {
        format!("cleanup incomplete for {case_path:?}; parent must retain image")
    })?;
    let evidence = outcome
        .with_context(|| format!("filesystem fault did not qualify; child stderr: {stderr}"))?;
    exit_request?;
    ensure!(
        !forced && status.success(),
        "child needed forced exit or failed: {status}; {stderr}"
    );
    recovery?;
    ensure!(Instant::now() <= overall, "overall120s deadline exceeded");
    println!(
        "receipt035-filesystem-evidence:{}",
        serde_json::to_string(&evidence)?
    );
    // Parent separately detaches its exact image. A pass here qualifies only
    // this observed filesystem/code/phase, not Linux or power-loss durability.
    Ok(())
}
