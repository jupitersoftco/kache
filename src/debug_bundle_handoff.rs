//! Deferred macOS debug bundles (`[cache] macos_debug_bundle = "deferred"`).
//!
//! A cached macOS executable with unpacked debug info is stored with a
//! `.dSYM` that `dsymutil` builds from the objects its `N_OSO` debug map
//! names (kunobi-ninja/kache#319). For a large test harness that is seconds
//! of work after the link, all of it on the build's critical path. This
//! module moves it to the daemon without changing what gets stored:
//!
//! - The wrapper clones (`clonefile`) the executable, every object or archive
//!   its debug map names under the link's `-oso_prefix` root, and the entry's
//!   other files into a staging directory under the store's handoff
//!   directory. The clones share blocks with the originals, so they are cheap,
//!   and they keep the link-time bytes when the next build rewrites the
//!   originals in place. A clone that cannot be made (another volume, a
//!   missing object) declines the deferral; the wrapper then runs `dsymutil`
//!   itself, as `sync` does.
//! - The wrapper sends [`PublishRustcDebugRequest`]. The daemon accepts by
//!   renaming the staging directory into a job directory; a wrapper whose
//!   wait ran out withdraws by renaming it first. Exactly one rename wins, so
//!   exactly one side owns the work.
//! - A daemon worker runs `dsymutil` at background priority from the job's
//!   `objects/` directory, the same relative layout the synchronous path
//!   reads from Cargo's profile directory. It points the bundle's recorded
//!   executable path back at the build's executable, places a copy of the
//!   bundle next to that executable if it is still the one that was linked
//!   (same `LC_UUID`), tars the bundle as `sync` does, and stores executable,
//!   other files and tar in one put. Restores see the same entry either way.
//! - Until that put the key has no entry: a build that needs it meanwhile
//!   misses and compiles. A bundle that fails is never replaced by an entry
//!   without one. Jobs are not persisted: a daemon that stops drops them, and
//!   the next daemon removes their directories before it accepts work.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

use crate::compiler::macho_debug_map::{oso_file, read_debug_map_at, uuid_hex};
use crate::config::Config;
use crate::daemon::{Daemon, Request, Response};
use crate::daemon_publish::{HandoffFile, handoff_dir};
use crate::store::{KeyLock, Store};

pub(crate) use kache_protocol::PublishRustcDebugRequest;

/// Every directory this module creates under the handoff directory starts
/// with this, so the orphan sweep reclaims what a crash leaves.
pub(crate) const DIR_PREFIX: &str = "dsym-";
/// A wrapper's staging directory, until the daemon renames it.
const STAGE_PREFIX: &str = "dsym-stage-";
/// A daemon job directory. Only a running daemon owns one.
const JOB_PREFIX: &str = "dsym-job-";
/// Suffix a wrapper renames its staging directory to when it withdraws.
const WITHDRAWN_SUFFIX: &str = ".withdrawn";

/// Executable clone inside a staging directory.
const EXECUTABLE_DIR: &str = "bin";
/// Mirror of the `-oso_prefix` root: `dsymutil` runs from here.
const OBJECTS_DIR: &str = "objects";
/// The entry's other files.
const FILES_DIR: &str = "files";
/// `dsymutil` output inside a job directory.
const OUTPUT_DIR: &str = "out";

/// Jobs at once. Each runs `dsymutil` with [`DSYMUTIL_THREADS`].
pub(crate) const WORKERS: usize = 2;
/// Accepted jobs waiting for a worker. A full queue declines, and that
/// wrapper builds its bundle itself.
pub(crate) const QUEUE_CAPACITY: usize = 8;
const DSYMUTIL_THREADS: &str = "2";
/// How long a wrapper waits for the daemon to accept. Acceptance is a rename;
/// the alternative is seconds of `dsymutil`.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a finished job waits for a peer that holds the key.
const PEER_WAIT: Duration = Duration::from_secs(30);
/// How long one `dsymutil` may run before it is killed, so a hung run cannot
/// hold a worker. The executable is then stored without a bundle.
const DSYMUTIL_TIMEOUT: Duration = Duration::from_secs(600);
/// Age after which a sibling `.<name>.dSYM.kache-*` directory is taken to be
/// one a stopped daemon left behind mid-install.
const INSTALL_LEFTOVER_AGE: Duration = Duration::from_secs(3600);

/// Whether this compile's bundle is one to defer: the configuration asks for
/// it, the host is macOS, and the executable's objects are still on disk
/// (`split-debuginfo=unpacked`). Packed debug info already has its `.dSYM`,
/// which the synchronous path only tars.
pub(crate) fn wanted(
    mode: crate::config::MacosDebugBundle,
    host_os: &str,
    split_debuginfo: Option<&str>,
) -> bool {
    mode == crate::config::MacosDebugBundle::Deferred
        && host_os == "macos"
        && split_debuginfo == Some("unpacked")
}

// ── Wrapper side ─────────────────────────────────────────────────

/// What the entry records besides its files.
pub(crate) struct EntryFields<'a> {
    pub(crate) cache_key: &'a str,
    /// The key whose lock the wrapper holds (see [`PublishRustcDebugRequest::lock_key`]).
    pub(crate) lock_key: &'a str,
    pub(crate) crate_name: &'a str,
    pub(crate) crate_types: &'a [String],
    pub(crate) features: &'a [String],
    pub(crate) target: &'a str,
    pub(crate) profile: &'a str,
    pub(crate) rustc_identity: Option<&'a str>,
    pub(crate) unit: Option<&'a str>,
    pub(crate) stdout: &'a str,
    pub(crate) stderr: &'a str,
    pub(crate) compile_time_ms: u64,
    pub(crate) publishes_to_remote: bool,
}

/// The executable a deferred bundle is built from.
pub(crate) struct LinkedExecutable<'a> {
    /// Absolute path the build left it at.
    pub(crate) path: &'a Path,
    /// Its name in the entry.
    pub(crate) store_name: &'a str,
    /// The `-oso_prefix` root its relative `N_OSO` paths resolve against.
    pub(crate) oso_root: &'a Path,
    /// The sysroot of the rustc that linked it. Its rlibs are the only
    /// absolute `N_OSO` paths a job may read where they are.
    pub(crate) sysroot: Option<&'a Path>,
    /// The `dsymutil` to run.
    pub(crate) dsymutil: &'a Dsymutil,
}

/// The `dsymutil` the synchronous path would run: the program as the build's
/// environment names it, and that whole environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dsymutil {
    pub(crate) program: String,
    pub(crate) env: Vec<(String, String)>,
}

impl Dsymutil {
    /// What `command` runs: its program and every variable it sets. The
    /// synchronous path's command clears the environment first, so these are
    /// all it sees. A program or variable that is not UTF-8 cannot travel in
    /// a request, and the caller keeps the synchronous path.
    pub(crate) fn from_command(command: &Command) -> Option<Self> {
        let program = command.get_program().to_str()?.to_owned();
        let mut env = Vec::new();
        for (name, value) in command.get_envs() {
            let Some(value) = value else {
                continue;
            };
            env.push((name.to_str()?.to_owned(), value.to_str()?.to_owned()));
        }
        Some(Self { program, env })
    }
}

/// Outcome of offering a deferred bundle to the daemon.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Handoff {
    /// The daemon owns the job, the staging directory and the store.
    Accepted,
    /// The wrapper stores as before; `reason` is for the debug log.
    Declined(String),
}

/// Clone what the job needs and offer it to the daemon. `files` are the
/// entry's files as the store would read them, `(path, store name)`; the
/// executable among them is replaced by its clone.
pub(crate) fn hand_off(
    config: &Config,
    entry: &EntryFields<'_>,
    files: &[(PathBuf, String)],
    executable: &LinkedExecutable<'_>,
) -> Handoff {
    let _trace = crate::phase_trace::phase("debug_bundle_handoff");
    let socket = config.socket_path();
    #[cfg(unix)]
    if !socket.exists() {
        return Handoff::Declined("no daemon socket".to_string());
    }
    let capture = match capture(&handoff_dir(config), files, executable, &clone_file) {
        Ok(capture) => capture,
        Err(error) => return Handoff::Declined(format!("capture failed: {error:#}")),
    };
    let request = request_for(entry, executable, &capture);
    let reply = crate::daemon::send_request_with_timeout(
        &socket,
        &Request::PublishRustcDebug(Box::new(request)),
        HANDOFF_TIMEOUT,
    );
    finish_handoff(capture.dir, reply)
}

/// A filled staging directory.
#[derive(Debug)]
struct Capture {
    dir: PathBuf,
    /// The executable clone, relative to `dir`.
    executable: String,
    /// Every entry file, relative to `dir`, in the order given.
    files: Vec<HandoffFile>,
    uuid: [u8; 16],
}

/// Removes a staging directory unless disarmed.
struct StagingGuard(Option<PathBuf>);

impl StagingGuard {
    fn disarm(mut self) -> PathBuf {
        self.0.take().expect("armed guard")
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Fill a new staging directory under `parent` with link-time clones.
/// `clone` makes each copy of the executable and its objects: copy-on-write
/// in production ([`clone_file`]).
fn capture(
    parent: &Path,
    files: &[(PathBuf, String)],
    executable: &LinkedExecutable<'_>,
    clone: &dyn Fn(&Path, &Path) -> Result<()>,
) -> Result<Capture> {
    let _trace = crate::phase_trace::phase("debug_bundle_capture");
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let guard = StagingGuard(Some(
        tempfile::Builder::new()
            .prefix(STAGE_PREFIX)
            .tempdir_in(parent)?
            .keep(),
    ));
    let dir = guard.0.clone().expect("armed guard");

    let file_name = executable
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .context("the executable has no usable file name")?;
    let executable_rel = format!("{EXECUTABLE_DIR}/{file_name}");
    let executable_clone = dir.join(&executable_rel);
    std::fs::create_dir(dir.join(EXECUTABLE_DIR))?;
    clone(executable.path, &executable_clone)?;

    // Read the clone, not the original: its debug map is the one `dsymutil`
    // will read, whatever happens to the original meanwhile.
    let Some(map) = read_debug_map_at(&executable_clone)? else {
        bail!("not a thin 64-bit Mach-O executable with a UUID");
    };
    let objects = dir.join(OBJECTS_DIR);
    std::fs::create_dir(&objects)?;
    for entry in &map.objects {
        let Some(relative) = object_to_capture(oso_file(entry), executable.sysroot)? else {
            continue;
        };
        let target = objects.join(relative);
        if target.exists() {
            continue; // another member of an archive already cloned
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        clone(&executable.oso_root.join(relative), &target)?;
    }

    std::fs::create_dir(dir.join(FILES_DIR))?;
    let mut staged = Vec::with_capacity(files.len());
    for (index, (path, store_name)) in files.iter().enumerate() {
        if path == executable.path {
            staged.push(HandoffFile {
                path: executable_rel.clone(),
                store_name: store_name.clone(),
            });
            continue;
        }
        let relative = format!("{FILES_DIR}/{index}-{}", flat_name(store_name));
        copy_file(path, &dir.join(&relative))?;
        staged.push(HandoffFile {
            path: relative,
            store_name: store_name.clone(),
        });
    }
    ensure!(
        staged.iter().any(|file| file.path == executable_rel),
        "the executable is not among the entry's files"
    );
    Ok(Capture {
        dir: guard.disarm(),
        executable: executable_rel,
        files: staged,
        uuid: map.uuid,
    })
}

/// The path to clone for one `N_OSO` file, relative to the `-oso_prefix`
/// root, or `None` for an rlib of the toolchain's sysroot, which no build
/// rewrites; `dsymutil` reads those where they are. Any other absolute path
/// (a static archive a build script wrote and may rewrite, or an object the
/// prefix did not strip) and a relative path that leaves the root decline
/// the deferral.
fn object_to_capture<'a>(file: &'a str, sysroot: Option<&Path>) -> Result<Option<&'a Path>> {
    let path = Path::new(file);
    if path.is_absolute() {
        ensure!(
            sysroot.is_some_and(|sysroot| in_sysroot(path, sysroot)),
            "{file} is outside the -oso_prefix root and the toolchain sysroot"
        );
        return Ok(None);
    }
    ensure!(is_plain_relative(path), "unexpected OSO path {file}");
    Ok(Some(path))
}

/// Under `sysroot` as spelled, or as it resolves through links.
fn in_sysroot(path: &Path, sysroot: &Path) -> bool {
    path.starts_with(sysroot)
        || std::fs::canonicalize(sysroot).is_ok_and(|real| path.starts_with(real))
}

/// Non-empty, and only normal components: no root, `.` or `..`.
fn is_plain_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// A store name as one path component.
fn flat_name(store_name: &str) -> String {
    store_name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

/// Copy-on-write clone; nothing else. A deferral that would copy gigabytes of
/// objects is slower than the `dsymutil` it moves off the build.
fn clone_file(source: &Path, target: &Path) -> Result<()> {
    kache_fs::try_reflink(source, target).with_context(|| {
        format!(
            "cloning {} (copy-on-write is required to defer)",
            source.display()
        )
    })
}

/// Clone, or copy where the volume cannot clone. For the entry's small
/// files, which may sit in a private temporary directory on another volume.
fn copy_file(source: &Path, target: &Path) -> Result<()> {
    if kache_fs::try_reflink(source, target).is_ok() {
        return Ok(());
    }
    std::fs::copy(source, target)
        .with_context(|| format!("copying {}", source.display()))
        .map(|_| ())
}

fn request_for(
    entry: &EntryFields<'_>,
    executable: &LinkedExecutable<'_>,
    capture: &Capture,
) -> PublishRustcDebugRequest {
    PublishRustcDebugRequest {
        client_version: Some(crate::VERSION.to_owned()),
        cache_key: entry.cache_key.to_owned(),
        lock_key: entry.lock_key.to_owned(),
        crate_name: entry.crate_name.to_owned(),
        crate_types: entry.crate_types.to_vec(),
        features: entry.features.to_vec(),
        target: entry.target.to_owned(),
        profile: entry.profile.to_owned(),
        rustc_identity: entry.rustc_identity.map(str::to_owned),
        unit: entry.unit.map(str::to_owned),
        stdout: entry.stdout.to_owned(),
        stderr: entry.stderr.to_owned(),
        compile_time_ms: entry.compile_time_ms,
        publishes_to_remote: entry.publishes_to_remote,
        staging_dir: capture.dir.to_string_lossy().into_owned(),
        files: capture.files.clone(),
        executable: capture.executable.clone(),
        bundle_store_name: format!("{}.dsym.tar", executable.store_name),
        installed_executable: executable.path.to_string_lossy().into_owned(),
        uuid: uuid_hex(&capture.uuid),
        dsymutil: executable.dsymutil.program.clone(),
        dsymutil_env: executable.dsymutil.env.clone(),
    }
}

/// Settle ownership of `dir` after the daemon's reply. An `ok` reply means
/// the daemon renamed it. Anything else, a timeout included, is resolved by
/// renaming it ourselves: if that finds it gone, the daemon took it.
fn finish_handoff(dir: PathBuf, reply: Result<String>) -> Handoff {
    let reason = match reply {
        Ok(line) => match serde_json::from_str::<Response>(&line) {
            Ok(response) if response.ok => return Handoff::Accepted,
            Ok(response) => response
                .error
                .unwrap_or_else(|| "daemon declined without a reason".to_string()),
            Err(error) => format!("unreadable daemon reply: {error}"),
        },
        Err(error) => format!("{error:#}"),
    };
    let mut withdrawn = dir.clone().into_os_string();
    withdrawn.push(WITHDRAWN_SUFFIX);
    match std::fs::rename(&dir, &withdrawn) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&withdrawn);
            Handoff::Declined(reason)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Handoff::Accepted,
        // Still in place, and the daemon may yet take it. Store here too: a
        // second put of the same key finds the first and stops.
        Err(error) => Handoff::Declined(format!("{reason}; withdrawing failed: {error}")),
    }
}

// ── Daemon side ──────────────────────────────────────────────────

/// An accepted job: the request, its paths now under `dir`.
pub(crate) struct DebugBundleJob {
    request: PublishRustcDebugRequest,
    dir: PathBuf,
    uuid: [u8; 16],
}

/// The daemon's queue of accepted jobs, drained by [`WORKERS`] threads.
pub(crate) struct DebugBundleQueue {
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<DebugBundleJob>>>,
    closing: Arc<std::sync::atomic::AtomicBool>,
}

impl DebugBundleQueue {
    pub(crate) fn new() -> Self {
        Self {
            tx: std::sync::Mutex::new(None),
            closing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn set_sender(&self, tx: tokio::sync::mpsc::Sender<DebugBundleJob>) {
        if let Ok(mut slot) = self.tx.lock() {
            *slot = Some(tx);
        }
    }

    fn sender(&self) -> Option<tokio::sync::mpsc::Sender<DebugBundleJob>> {
        self.tx.lock().ok().and_then(|slot| slot.clone())
    }

    /// Refuse new jobs and drop queued ones. A running `dsymutil` finishes,
    /// but nothing more is stored: an unstored entry is only a later miss.
    pub(crate) fn close(&self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut slot) = self.tx.lock() {
            slot.take();
        }
    }

    fn is_closing(&self) -> bool {
        self.closing.load(std::sync::atomic::Ordering::Acquire)
    }
}

fn refused(reason: &str) -> Response {
    Response::err(format!("debug bundle refused: {reason}"))
}

/// Why `request` cannot be a job, or the executable's UUID when it can.
fn validate(config: &Config, request: &PublishRustcDebugRequest) -> Result<[u8; 16], String> {
    if !crate::cache_key::is_valid_cache_key(&request.cache_key)
        || !crate::cache_key::is_valid_cache_key(&request.lock_key)
    {
        return Err("invalid cache key".to_string());
    }
    if request.dsymutil.is_empty() {
        return Err("no dsymutil".to_string());
    }
    let staging = Path::new(&request.staging_dir);
    let owned = staging.parent() == Some(handoff_dir(config).as_path())
        && staging
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(STAGE_PREFIX) && !name.contains(".."));
    if !owned {
        return Err(format!(
            "{} is not a staging directory of this store",
            request.staging_dir
        ));
    }
    if let Some(file) = request
        .files
        .iter()
        .find(|file| !is_plain_relative(Path::new(&file.path)))
    {
        return Err(format!("{} is not inside the staging directory", file.path));
    }
    if !request
        .files
        .iter()
        .any(|file| file.path == request.executable)
    {
        return Err("the executable is not among the files".to_string());
    }
    if request.bundle_store_name.is_empty() || request.bundle_store_name.contains(['/', '\\']) {
        return Err(format!(
            "invalid bundle name {:?}",
            request.bundle_store_name
        ));
    }
    if !Path::new(&request.installed_executable).is_absolute() {
        return Err("the installed executable path is not absolute".to_string());
    }
    parse_uuid(&request.uuid).ok_or_else(|| format!("invalid uuid {:?}", request.uuid))
}

fn parse_uuid(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut uuid = [0_u8; 16];
    for (index, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(uuid)
}

impl Daemon {
    /// Take a wrapper's staging directory if the request is sound and the
    /// queue has room. The rename is the acceptance: from then on the
    /// wrapper's withdrawal finds nothing to rename and leaves the job here.
    pub(crate) fn handle_publish_rustc_debug(&self, request: PublishRustcDebugRequest) -> Response {
        let config = self.config();
        let uuid = match validate(config, &request) {
            Ok(uuid) => uuid,
            Err(reason) => return refused(&reason),
        };
        let Some(tx) = self.debug_bundle_queue().sender() else {
            return refused("queue closed");
        };
        let permit = match tx.try_reserve_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => return refused("queue full"),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return refused("queue closed");
            }
        };
        match take_staging(&handoff_dir(config), Path::new(&request.staging_dir)) {
            Ok(dir) => {
                permit.send(DebugBundleJob { request, dir, uuid });
                Response::ok()
            }
            Err(error) => refused(&format!("{error:#}")),
        }
    }
}

/// Rename `staging` to a new job directory under `parent`.
fn take_staging(parent: &Path, staging: &Path) -> Result<PathBuf> {
    // A unique empty directory; renaming a directory onto an empty one
    // replaces it.
    let job = tempfile::Builder::new()
        .prefix(JOB_PREFIX)
        .tempdir_in(parent)?
        .keep();
    if let Err(error) = std::fs::rename(staging, &job) {
        let _ = std::fs::remove_dir(&job);
        return Err(error).context("taking the staging directory");
    }
    Ok(job)
}

/// Remove every job directory. Called before a daemon accepts jobs: any job
/// directory then belongs to a daemon that stopped, and its job is gone.
pub(crate) fn remove_abandoned_jobs(config: &Config) -> usize {
    let Ok(entries) = std::fs::read_dir(handoff_dir(config)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_job = entry.file_name().to_string_lossy().starts_with(JOB_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_job && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Start the workers. Each has its own store connection and runs one job
/// at a time; they share the queue.
pub(crate) fn spawn_workers(
    daemon: Arc<Daemon>,
    rx: tokio::sync::mpsc::Receiver<DebugBundleJob>,
) -> std::io::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let rx = Arc::new(std::sync::Mutex::new(rx));
    for index in 0..WORKERS {
        let daemon = Arc::clone(&daemon);
        let rx = Arc::clone(&rx);
        let runtime = runtime.clone();
        std::thread::Builder::new()
            .name(format!("kache-dsym-{index}"))
            .spawn(move || {
                // Uploads are queued through the runtime, as for cc publishes.
                let _runtime = runtime.enter();
                lower_thread_priority();
                run_worker(&daemon, &rx);
            })?;
    }
    Ok(())
}

fn run_worker(
    daemon: &Arc<Daemon>,
    rx: &std::sync::Mutex<tokio::sync::mpsc::Receiver<DebugBundleJob>>,
) {
    let config = daemon.config().clone();
    let store = Store::open(&config);
    while let Some(job) = rx.lock().ok().and_then(|mut rx| rx.blocking_recv()) {
        match &store {
            Ok(store) => {
                process_job(
                    daemon,
                    &config,
                    store,
                    job,
                    &dsymutil_command,
                    DSYMUTIL_TIMEOUT,
                );
            }
            Err(error) => {
                let _cleanup = StagingGuard(Some(job.dir));
                tracing::warn!("debug bundle worker has no store: {error:#}");
            }
        }
    }
}

/// Builds the `dsymutil` command for a job: the request, then the staged
/// executable, the bundle to write and the directory to run in.
type DsymutilCommand = dyn Fn(&PublishRustcDebugRequest, &Path, &Path, &Path) -> Command;

/// How one job ended.
#[derive(Debug, PartialEq, Eq)]
enum JobOutcome {
    /// Executable and bundle are stored as one entry.
    Stored,
    /// The bundle failed and the executable is stored without one, as the
    /// synchronous path does.
    StoredWithoutBundle,
    /// A peer stored the key first.
    PeerStored,
    /// The put failed; nothing is stored.
    Failed,
    /// The daemon is stopping; nothing is stored.
    Dropped,
}

/// Run one job to its end. Its directory is gone afterwards, whatever the
/// outcome.
fn process_job(
    daemon: &Arc<Daemon>,
    config: &Config,
    store: &Store,
    job: DebugBundleJob,
    dsymutil: &DsymutilCommand,
    timeout: Duration,
) -> JobOutcome {
    let _cleanup = StagingGuard(Some(job.dir.clone()));
    let crate_name = &job.request.crate_name;
    let started = std::time::Instant::now();
    if daemon.debug_bundle_queue().is_closing() {
        return JobOutcome::Dropped;
    }
    let (files, bundled) = entry_files(&job, dsymutil, timeout);
    if daemon.debug_bundle_queue().is_closing() {
        return JobOutcome::Dropped;
    }
    let put = publish(store, &job.request, &files);
    crate::wrapper::after_store(config, store, put.as_ref().err());
    match put {
        Ok(true) => {
            if job.request.publishes_to_remote && config.remote.is_some() {
                crate::daemon_publish::enqueue_entry_upload(
                    daemon,
                    config,
                    &job.request.cache_key,
                    crate_name,
                );
            }
            tracing::info!(
                "stored {crate_name} {} its deferred debug bundle after {} ms",
                if bundled { "with" } else { "without" },
                started.elapsed().as_millis()
            );
            if bundled {
                JobOutcome::Stored
            } else {
                JobOutcome::StoredWithoutBundle
            }
        }
        Ok(false) => {
            tracing::debug!("{crate_name}: a peer stored the key first");
            JobOutcome::PeerStored
        }
        Err(error) => {
            tracing::warn!(
                "could not store {crate_name} from its deferred debug bundle job: {}",
                crate::wrapper::store_error_for_event(&error)
            );
            JobOutcome::Failed
        }
    }
}

/// The entry's files, `(absolute path, store name)`, with the bundle tar when
/// the bundle could be built, and whether it could. A bundle that fails
/// leaves the executable to be stored without one, with a warning: what the
/// synchronous path does.
fn entry_files(
    job: &DebugBundleJob,
    dsymutil: &DsymutilCommand,
    timeout: Duration,
) -> (Vec<(PathBuf, String)>, bool) {
    let mut files: Vec<(PathBuf, String)> = job
        .request
        .files
        .iter()
        .map(|file| (job.dir.join(&file.path), file.store_name.clone()))
        .collect();
    match build_bundle(job, dsymutil, timeout) {
        Ok(tar) => {
            files.push((tar, job.request.bundle_store_name.clone()));
            (files, true)
        }
        Err(error) => {
            tracing::warn!(
                "deferred debug bundle for {} failed, caching it without one: {error:#}",
                job.request.crate_name
            );
            (files, false)
        }
    }
}

/// Run `dsymutil`, place the bundle next to the build's executable, and tar
/// it. Returns the tar.
fn build_bundle(
    job: &DebugBundleJob,
    dsymutil: &DsymutilCommand,
    timeout: Duration,
) -> Result<PathBuf> {
    let request = &job.request;
    let executable = job.dir.join(&request.executable);
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("the executable has no file name")?
        .to_owned();
    let output = job.dir.join(OUTPUT_DIR);
    std::fs::create_dir_all(&output)?;
    let objects = job.dir.join(OBJECTS_DIR);
    std::fs::create_dir_all(&objects)?;
    let bundle = output.join(format!("{name}.dSYM"));
    run_bounded(
        dsymutil(request, &executable, &bundle, &objects),
        &job.dir.join("dsymutil.log"),
        timeout,
    )?;
    let installed = Path::new(&request.installed_executable);
    point_relocations_at(&bundle, &executable, installed)?;
    match install_beside(&bundle, installed, &job.uuid) {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            "{} was replaced after its link; its bundle is only stored",
            installed.display()
        ),
        Err(error) => tracing::debug!(
            "could not place the bundle next to {}: {error:#}",
            installed.display()
        ),
    }
    let tar = job.dir.join(&request.bundle_store_name);
    crate::compiler::platform::build_deterministic_tar(&bundle, &tar)?;
    Ok(tar)
}

/// Run `command` to completion within `timeout`, its stderr in `log`. A run
/// past the timeout is killed. An exit other than success is an error that
/// carries the end of `log`.
fn run_bounded(mut command: Command, log: &Path, timeout: Duration) -> Result<()> {
    let stderr = std::fs::File::create(log).context("creating the dsymutil log")?;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("starting dsymutil")?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for dsymutil")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "dsymutil ran longer than {} s and was killed",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = std::fs::read_to_string(log).unwrap_or_default();
    let output = output.trim();
    if !status.success() {
        let tail = &output[output.len().saturating_sub(4096)..];
        bail!("dsymutil exited with {status}: {tail}");
    }
    if !output.is_empty() {
        tracing::debug!("dsymutil: {output}");
    }
    Ok(())
}

/// The command the synchronous path runs (`dsymutil <executable> -o
/// <bundle>`, the build's program and environment), from `objects`, the
/// mirror of the `-oso_prefix` root. Adds `-j 2` and clamps it to background
/// priority where macOS offers `taskpolicy`.
fn dsymutil_command(
    request: &PublishRustcDebugRequest,
    executable: &Path,
    bundle: &Path,
    objects: &Path,
) -> Command {
    const TASKPOLICY: &str = "/usr/sbin/taskpolicy";
    let mut command = if Path::new(TASKPOLICY).exists() {
        let mut command = Command::new(TASKPOLICY);
        command.args(["-c", "background"]).arg(&request.dsymutil);
        command
    } else {
        Command::new(&request.dsymutil)
    };
    command
        .env_clear()
        .envs(
            request
                .dsymutil_env
                .iter()
                .map(|(name, value)| (name, value)),
        )
        .args(["-j", DSYMUTIL_THREADS])
        .arg(executable)
        .arg("-o")
        .arg(bundle)
        .current_dir(objects);
    command
}

/// `dsymutil` records the path it read the executable from in each
/// `Contents/Resources/Relocations/<arch>/<name>.yml`. Name the executable
/// where the build left it, as the synchronous path does, so both produce
/// the same bundle bytes.
fn point_relocations_at(bundle: &Path, read_from: &Path, installed: &Path) -> Result<()> {
    let Ok(arches) = std::fs::read_dir(bundle.join("Contents/Resources/Relocations")) else {
        return Ok(());
    };
    let (from, to) = (read_from.to_string_lossy(), installed.to_string_lossy());
    for arch in arches {
        for file in std::fs::read_dir(arch?.path())? {
            let path = file?.path();
            if path.extension().is_none_or(|ext| ext != "yml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if let Some(rewritten) = rewrite_binary_path(&text, &from, &to) {
                std::fs::write(&path, rewritten)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// `text` with its `binary-path:` value `from` replaced by `to`, or `None`
/// when no such line is there. The value is matched in each spelling LLVM's
/// YAML writer can give it, and `to` is written the way that writer would.
fn rewrite_binary_path(text: &str, from: &str, to: &str) -> Option<String> {
    let spellings = [yaml_single_quoted(from), yaml_double_quoted(from)];
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        if let Some(value) = body.strip_prefix("binary-path:") {
            let padding = &value[..value.len() - value.trim_start().len()];
            let value = value.trim();
            if value == from || spellings.iter().any(|spelling| spelling == value) {
                out.push_str("binary-path:");
                out.push_str(padding);
                out.push_str(&yaml_scalar(to));
                out.push_str(ending);
                changed = true;
                continue;
            }
        }
        out.push_str(line);
    }
    changed.then_some(out)
}

/// `value` as LLVM's YAML writer quotes a path (`yaml::needsQuotes`): any
/// byte outside ASCII, `DEL` or a control character other than tab, line feed
/// and carriage return needs double quotes; otherwise single quotes, which a
/// path's `/` always needs.
fn yaml_scalar(value: &str) -> String {
    let double = value
        .bytes()
        .any(|byte| byte >= 0x7f || (byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r')));
    if double {
        yaml_double_quoted(value)
    } else {
        yaml_single_quoted(value)
    }
}

fn yaml_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// LLVM's `yaml::escape` without escaping printable characters.
fn yaml_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\0' => out.push_str("\\0"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\e"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02X}", c as u32)),
            '\u{85}' => out.push_str("\\N"),
            '\u{a0}' => out.push_str("\\_"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Put a copy of `bundle` at `<installed>.dSYM` if `installed` still has
/// `uuid`, replacing whatever bundle is there in one rename. `Ok(false)`
/// when the executable is gone or was relinked, checked before the copy and
/// again just before the rename: its own job, or a restore, supplies its
/// bundle. A bundle that lands after a relink anyway is harmless, because
/// lldb ignores a `.dSYM` whose UUID does not match.
fn install_beside(bundle: &Path, installed: &Path, uuid: &[u8; 16]) -> Result<bool> {
    if !has_uuid(installed, uuid) {
        return Ok(false);
    }
    let name = installed
        .file_name()
        .and_then(|name| name.to_str())
        .context("the executable has no file name")?;
    let parent = installed
        .parent()
        .context("the executable has no parent directory")?;
    let prefix = format!(".{name}.dSYM.kache-");
    remove_install_leftovers(parent, &prefix, INSTALL_LEFTOVER_AGE);
    let staged = StagingGuard(Some(
        tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir_in(parent)?
            .keep(),
    ));
    let staged_path = staged.0.clone().expect("armed guard");
    // tempfile creates 0700; a `.dSYM` is read by whoever debugs the build.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged_path, std::fs::Permissions::from_mode(0o755))?;
    }
    copy_tree(bundle, &staged_path)?;
    if !has_uuid(installed, uuid) {
        return Ok(false);
    }
    exchange_into_place(
        &staged_path,
        &installed.with_file_name(format!("{name}.dSYM")),
    )?;
    // `staged_path` now holds the replaced bundle, if there was one; the
    // guard removes it.
    drop(staged);
    Ok(true)
}

fn has_uuid(executable: &Path, uuid: &[u8; 16]) -> bool {
    matches!(read_debug_map_at(executable), Ok(Some(map)) if map.uuid == *uuid)
}

/// Remove `prefix` directories in `dir` older than `age`: what an install
/// leaves when its daemon stops between the copy and the rename. Only this
/// executable's own prefix is matched, and a younger one may be another
/// job's install in progress.
fn remove_install_leftovers(dir: &Path, prefix: &str, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|elapsed| elapsed >= age)
            });
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if ours && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Copy the tree under `source` into the existing directory `target`.
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir(&to)?;
            copy_tree(&entry.path(), &to)?;
        } else {
            copy_file(&entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Move `staged` to `dest`. When `dest` already exists the two trade places,
/// atomically on macOS, so a reader sees the old bundle or the new one.
fn exchange_into_place(staged: &Path, dest: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dest).is_err() {
        return std::fs::rename(staged, dest)
            .with_context(|| format!("moving the bundle to {}", dest.display()));
    }
    swap(staged, dest).with_context(|| format!("replacing {}", dest.display()))
}

#[cfg(target_os = "macos")]
fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Not atomic; only tests run this off macOS.
#[cfg(not(target_os = "macos"))]
fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    let mut aside = b.as_os_str().to_owned();
    aside.push(".kache-swap");
    std::fs::rename(b, &aside)?;
    std::fs::rename(a, b)?;
    std::fs::rename(&aside, a)
}

/// Store the job's files as one entry, unless a peer already stored or is
/// storing the key. `Ok(false)` when it was not ours to store.
fn publish(
    store: &Store,
    request: &PublishRustcDebugRequest,
    files: &[(PathBuf, String)],
) -> Result<bool> {
    let Some(_lock) = claim(store, &request.lock_key, &request.cache_key, PEER_WAIT)? else {
        return Ok(false);
    };
    store.put_rustc_with_compile_time(
        &request.cache_key,
        &request.crate_name,
        &request.crate_types,
        &request.features,
        &request.target,
        &request.profile,
        files,
        &request.stdout,
        &request.stderr,
        request.compile_time_ms,
        request.rustc_identity.as_deref(),
    )?;
    if let Some(unit) = request.unit.as_deref()
        && let Err(error) = store.record_entry_unit(&request.cache_key, unit)
    {
        tracing::debug!(
            "recording the unit of {}'s entry failed: {error}",
            request.crate_name
        );
    }
    Ok(true)
}

/// The lock the wrapper held, on `lock_key` (the shared key, which also
/// covers a width key), or `None` when `entry_key` is already stored. A peer
/// still holding the lock gets `wait` to finish.
fn claim(
    store: &Store,
    lock_key: &str,
    entry_key: &str,
    wait: Duration,
) -> Result<Option<KeyLock>> {
    let lock = match store.try_lock(lock_key)? {
        Some(lock) => lock,
        None => {
            store.wait_for_committed_with_timeout(lock_key, wait)?;
            match store.try_lock(lock_key)? {
                Some(lock) => lock,
                None => return Ok(None),
            }
        }
    };
    if store.contains(entry_key) {
        return Ok(None);
    }
    Ok(Some(lock))
}

/// Utility priority for the worker's own tar and store work; `dsymutil` is
/// clamped further by [`dsymutil_command`].
#[cfg(target_os = "macos")]
fn lower_thread_priority() {
    const QOS_CLASS_UTILITY: libc::c_uint = 0x11;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(
            qos_class: libc::c_uint,
            relative_priority: libc::c_int,
        ) -> libc::c_int;
    }
    // SAFETY: changes only the calling thread's scheduling class.
    let _ = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) };
}

#[cfg(not(target_os = "macos"))]
fn lower_thread_priority() {}

#[cfg(test)]
#[path = "debug_bundle_handoff/tests.rs"]
mod tests;
