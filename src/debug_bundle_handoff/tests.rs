use super::*;
use crate::compiler::macho_debug_map::tests::synthetic_macho;
use crate::config::MacosDebugBundle;

const N_OSO: u8 = 0x66;
const UUID: [u8; 16] = [7; 16];
const OTHER_UUID: [u8; 16] = [9; 16];

fn key(label: &str) -> String {
    blake3::hash(label.as_bytes()).to_hex().to_string()
}

/// Whether `dir`'s volume clones files, which [`capture`] requires. Linux
/// tmpfs does not; APFS does.
fn clones_files(dir: &Path) -> bool {
    let source = dir.join(".probe-source");
    let target = dir.join(".probe-target");
    std::fs::write(&source, b"probe").unwrap();
    let cloned = kache_fs::try_reflink(&source, &target).is_ok();
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&target);
    if !cloned {
        eprintln!("skipping: {} cannot clone files", dir.display());
    }
    cloned
}

#[test]
fn only_unpacked_debug_info_on_macos_under_deferred_is_wanted() {
    assert!(wanted(
        MacosDebugBundle::Deferred,
        "macos",
        Some("unpacked")
    ));
    assert!(!wanted(MacosDebugBundle::Sync, "macos", Some("unpacked")));
    assert!(!wanted(
        MacosDebugBundle::Deferred,
        "linux",
        Some("unpacked")
    ));
    for split in [None, Some("packed"), Some("off")] {
        assert!(
            !wanted(MacosDebugBundle::Deferred, "macos", split),
            "{split:?}"
        );
    }
}

#[test]
fn only_relative_objects_inside_the_root_are_captured() {
    let root = Path::new("/target/debug");
    assert_eq!(
        object_to_capture("deps/a.o", root).unwrap(),
        Some(Path::new("deps/a.o"))
    );
    assert_eq!(
        object_to_capture("/toolchain/lib/libstd.rlib", root).unwrap(),
        None
    );
    assert!(object_to_capture("/target/debug/deps/a.o", root).is_err());
    for escaping in ["../other/a.o", "./a.o", "deps/../../a.o", ""] {
        assert!(object_to_capture(escaping, root).is_err(), "{escaping:?}");
    }
}

#[test]
fn store_names_flatten_to_one_component() {
    assert_eq!(flat_name("foo.d"), "foo.d");
    assert_eq!(flat_name("a/b\\c"), "a_b_c");
}

/// A Cargo-shaped profile directory: an executable in `deps/` linked with
/// `-oso_prefix <profile>/`, an object, an rlib with two members, a
/// toolchain rlib outside the profile, and a dep-info file elsewhere.
struct Fixture {
    _dir: tempfile::TempDir,
    profile: PathBuf,
    handoff: PathBuf,
    executable: PathBuf,
    depinfo: PathBuf,
}

fn make_fixture(objects: &[&str]) -> Option<Fixture> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    if !clones_files(&root) {
        return None;
    }
    let profile = root.join("target/debug");
    let deps = profile.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    std::fs::write(deps.join("a.o"), b"object a").unwrap();
    std::fs::write(deps.join("libb.rlib"), b"archive b").unwrap();
    let mut symbols = vec![(0x0f_u8, "_main")];
    symbols.extend(objects.iter().map(|object| (N_OSO, *object)));
    let executable = deps.join("demo-1234");
    std::fs::write(&executable, synthetic_macho(Some(UUID), &symbols)).unwrap();
    let depinfo = root.join("private-demo.d");
    std::fs::write(&depinfo, b"demo: src/main.rs\n").unwrap();
    Some(Fixture {
        handoff: root.join("cache/staging/handoff"),
        _dir: dir,
        profile,
        executable,
        depinfo,
    })
}

const OBJECTS: &[&str] = &[
    "deps/a.o",
    "deps/libb.rlib(b1.o)",
    "deps/libb.rlib(b2.o)",
    "/toolchain/libstd.rlib(std.o)",
];

impl Fixture {
    fn files(&self) -> Vec<(PathBuf, String)> {
        vec![
            (self.executable.clone(), "demo-1234".to_string()),
            (self.depinfo.clone(), "demo-1234.d".to_string()),
        ]
    }

    fn linked(&self) -> LinkedExecutable<'_> {
        LinkedExecutable {
            path: &self.executable,
            store_name: "demo-1234",
            oso_root: &self.profile,
        }
    }

    fn staging_dirs(&self) -> Vec<String> {
        std::fs::read_dir(&self.handoff)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[test]
fn capture_clones_the_executable_its_objects_and_the_entry_files() {
    let Some(fixture) = make_fixture(OBJECTS) else {
        return;
    };
    let capture = capture(&fixture.handoff, &fixture.files(), &fixture.linked()).unwrap();

    assert_eq!(capture.dir.parent(), Some(fixture.handoff.as_path()));
    assert!(
        capture
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
    );
    assert_eq!(capture.uuid, UUID);
    assert_eq!(capture.executable, "bin/demo-1234");
    assert_eq!(
        capture.files,
        [
            HandoffFile {
                path: "bin/demo-1234".to_string(),
                store_name: "demo-1234".to_string(),
            },
            HandoffFile {
                path: "files/1-demo-1234.d".to_string(),
                store_name: "demo-1234.d".to_string(),
            },
        ]
    );
    let read = |relative: &str| std::fs::read(capture.dir.join(relative)).unwrap();
    assert_eq!(
        read("bin/demo-1234"),
        std::fs::read(&fixture.executable).unwrap()
    );
    assert_eq!(read("objects/deps/a.o"), b"object a");
    assert_eq!(read("objects/deps/libb.rlib"), b"archive b");
    assert_eq!(read("files/1-demo-1234.d"), b"demo: src/main.rs\n");
    assert!(!capture.dir.join("objects/toolchain").exists());

    // The clones keep the link-time bytes when the build rewrites its own.
    std::fs::write(fixture.profile.join("deps/a.o"), b"rewritten").unwrap();
    assert_eq!(read("objects/deps/a.o"), b"object a");
}

#[test]
fn a_capture_that_fails_leaves_nothing_behind() {
    let failing = |fixture: &Fixture, files: &[(PathBuf, String)], expected: &str| {
        let error = capture(&fixture.handoff, files, &fixture.linked()).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(
            fixture.staging_dirs().is_empty(),
            "{:?}",
            fixture.staging_dirs()
        );
    };

    let Some(missing) = make_fixture(&["deps/missing.o"]) else {
        return;
    };
    failing(&missing, &missing.files(), "missing.o");

    let Some(escaping) = make_fixture(&["deps/../escape.o"]) else {
        return;
    };
    failing(&escaping, &escaping.files(), "unexpected OSO path");

    let Some(unprefixed) = make_fixture(&[]) else {
        return;
    };
    let inside = unprefixed.profile.join("deps/a.o");
    let symbols = [(N_OSO, inside.to_str().unwrap())];
    std::fs::write(
        &unprefixed.executable,
        synthetic_macho(Some(UUID), &symbols),
    )
    .unwrap();
    failing(&unprefixed, &unprefixed.files(), "not relative");

    let Some(script) = make_fixture(OBJECTS) else {
        return;
    };
    std::fs::write(&script.executable, b"#!/bin/sh\n").unwrap();
    failing(&script, &script.files(), "Mach-O");

    let Some(unlisted) = make_fixture(OBJECTS) else {
        return;
    };
    failing(&unlisted, &unlisted.files()[1..], "not among");
}

fn reply(response: &Response) -> Result<String> {
    Ok(serde_json::to_string(response).unwrap())
}

#[test]
fn an_ok_reply_leaves_the_directory_to_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("dsym-stage-a");
    std::fs::create_dir(&staging).unwrap();
    assert_eq!(
        finish_handoff(staging.clone(), reply(&Response::ok())),
        Handoff::Accepted
    );
    assert!(staging.exists(), "only the daemon's rename moves it");
}

#[test]
fn a_refusal_withdraws_and_removes_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    for (label, reply) in [
        (
            "refused",
            reply(&Response::err("debug bundle refused: queue full")),
        ),
        ("unreadable", Ok("not json".to_string())),
        ("timed out", Err(anyhow::anyhow!("timed out"))),
    ] {
        let staging = dir
            .path()
            .join(format!("dsym-stage-{}", label.replace(' ', "-")));
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("file"), b"x").unwrap();
        let Handoff::Declined(reason) = finish_handoff(staging.clone(), reply) else {
            panic!("{label}: declined");
        };
        assert!(!reason.is_empty(), "{label}");
        assert!(!staging.exists(), "{label}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "{label}: withdrawn directory removed"
        );
    }
    let staging = dir.path().join("dsym-stage-quiet");
    std::fs::create_dir(&staging).unwrap();
    let mut silent = Response::ok();
    silent.ok = false;
    assert_eq!(
        finish_handoff(staging, reply(&silent)),
        Handoff::Declined("daemon declined without a reason".to_string())
    );
}

#[test]
fn a_directory_the_daemon_already_took_is_accepted_whatever_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let taken = dir.path().join("dsym-stage-taken");
    assert_eq!(
        finish_handoff(taken, Err(anyhow::anyhow!("timed out"))),
        Handoff::Accepted
    );
}

#[cfg(unix)]
#[test]
fn a_withdrawal_that_cannot_rename_still_declines() {
    use std::os::unix::fs::PermissionsExt as _;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root renames in a read-only directory
    }
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("locked");
    let staging = parent.join("dsym-stage-a");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = finish_handoff(staging.clone(), Err(anyhow::anyhow!("timed out")));
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Handoff::Declined(reason) = outcome else {
        panic!("declined");
    };
    assert!(reason.contains("withdrawing failed"), "{reason}");
    assert!(staging.exists(), "left for the sweep");
}

fn request(config: &Config, staging: &Path) -> PublishRustcDebugRequest {
    PublishRustcDebugRequest {
        client_version: Some(crate::VERSION.to_owned()),
        cache_key: key("demo"),
        crate_name: "demo".to_string(),
        crate_types: vec!["bin".to_string()],
        features: vec![],
        target: "host".to_string(),
        profile: "dev".to_string(),
        rustc_identity: Some("1.95.0 (fixture)".to_string()),
        unit: Some("1234".to_string()),
        stdout: String::new(),
        stderr: "warning: w\n".to_string(),
        compile_time_ms: 9,
        publishes_to_remote: false,
        staging_dir: staging.to_string_lossy().into_owned(),
        files: vec![
            HandoffFile {
                path: "bin/demo-1234".to_string(),
                store_name: "demo-1234".to_string(),
            },
            HandoffFile {
                path: "files/1-demo-1234.d".to_string(),
                store_name: "demo-1234.d".to_string(),
            },
        ],
        executable: "bin/demo-1234".to_string(),
        bundle_store_name: "demo-1234.dsym.tar".to_string(),
        installed_executable: handoff_dir(config)
            .join("../../target/demo-1234")
            .to_string_lossy()
            .into_owned(),
        uuid: uuid_hex(&UUID),
    }
}

#[test]
fn only_a_sound_request_validates() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let staging = handoff_dir(&config).join("dsym-stage-abc");
    let good = request(&config, &staging);
    assert_eq!(validate(&config, &good), Ok(UUID));

    type Spoil = fn(&mut PublishRustcDebugRequest);
    let invalid: [(&str, Spoil); 12] = [
        ("key", |r| r.cache_key = "../x".to_string()),
        ("outside", |r| {
            r.staging_dir = "/tmp/dsym-stage-abc".to_string()
        }),
        ("prefix", |r| {
            r.staging_dir = r.staging_dir.replace("dsym-stage-", "cc-");
        }),
        ("dotdot", |r| r.staging_dir.push_str("/..")),
        ("absolute file", |r| {
            r.files[1].path = "/etc/passwd".to_string()
        }),
        ("escaping file", |r| r.files[1].path = "../x".to_string()),
        ("executable", |r| r.executable = "bin/other".to_string()),
        ("empty bundle", |r| r.bundle_store_name.clear()),
        ("nested bundle", |r| {
            r.bundle_store_name = "a/b.tar".to_string()
        }),
        ("windows bundle", |r| {
            r.bundle_store_name = "a\\b.tar".to_string()
        }),
        ("relative install", |r| {
            r.installed_executable = "target/demo".to_string();
        }),
        ("uuid", |r| r.uuid = "xyz".to_string()),
    ];
    for (label, spoil) in invalid {
        let mut request = good.clone();
        spoil(&mut request);
        assert!(validate(&config, &request).is_err(), "{label}");
    }
}

#[test]
fn uuids_parse_from_exactly_32_hex_digits() {
    assert_eq!(parse_uuid(&uuid_hex(&UUID)), Some(UUID));
    assert_eq!(
        parse_uuid("0123456789ABCDEF0123456789abcdef").map(|uuid| uuid[15]),
        Some(0xef)
    );
    for bad in [
        "",
        "0123",
        &"0".repeat(31),
        &"0".repeat(33),
        &"g".repeat(32),
    ] {
        assert_eq!(parse_uuid(bad), None, "{bad:?}");
    }
    // 32 bytes but not 32 ASCII digits.
    assert_eq!(parse_uuid(&format!("{}é", "0".repeat(30))), None);
}

#[test]
fn taking_the_staging_directory_moves_it_into_a_job_directory() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("dsym-stage-a");
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("file"), b"x").unwrap();
    let job = take_staging(dir.path(), &staging).unwrap();
    assert!(!staging.exists());
    assert_eq!(std::fs::read(job.join("file")).unwrap(), b"x");
    assert!(
        job.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(JOB_PREFIX)
    );

    // A withdrawn directory cannot be taken, and no job directory is left.
    assert!(take_staging(dir.path(), &staging).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn only_job_directories_are_removed_as_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    assert_eq!(remove_abandoned_jobs(&config), 0, "no handoff dir yet");
    let handoff = handoff_dir(&config);
    for name in ["dsym-job-a", "dsym-job-b", "dsym-stage-c", "cc-d"] {
        std::fs::create_dir_all(handoff.join(name)).unwrap();
        std::fs::write(handoff.join(name).join("f"), b"x").unwrap();
    }
    std::fs::write(handoff.join("dsym-job-file"), b"x").unwrap();
    assert_eq!(remove_abandoned_jobs(&config), 2);
    let mut left: Vec<_> = std::fs::read_dir(&handoff)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["cc-d", "dsym-job-file", "dsym-stage-c"]);
}

#[test]
fn the_daemon_takes_a_sound_request_into_its_queue() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let daemon = Daemon::new(config.clone());
    let staging = handoff_dir(&config).join("dsym-stage-abc");
    std::fs::create_dir_all(&staging).unwrap();

    let closed = daemon.handle_publish_rustc_debug(request(&config, &staging));
    assert!(closed.error.unwrap().contains("queue closed"));

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    daemon.debug_bundle_queue().set_sender(tx);
    let mut unsound = request(&config, &staging);
    unsound.uuid.clear();
    assert!(
        daemon
            .handle_publish_rustc_debug(unsound)
            .error
            .unwrap()
            .contains("invalid uuid")
    );
    assert!(
        staging.exists(),
        "a refusal leaves the directory to the wrapper"
    );

    let accepted = daemon.handle_publish_rustc_debug(request(&config, &staging));
    assert!(accepted.ok, "{:?}", accepted.error);
    assert!(!staging.exists());
    let job = rx.try_recv().unwrap();
    assert_eq!(job.uuid, UUID);
    assert!(job.dir.is_dir());
    assert_eq!(job.request.cache_key, key("demo"));

    std::fs::create_dir_all(&staging).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    daemon.debug_bundle_queue().set_sender(tx.clone());
    assert!(
        daemon
            .handle_publish_rustc_debug(request(&config, &staging))
            .ok
    );
    std::fs::create_dir_all(&staging).unwrap();
    let full = daemon.handle_publish_rustc_debug(request(&config, &staging));
    assert!(full.error.unwrap().contains("queue full"));
    assert!(staging.exists());

    let gone = handoff_dir(&config).join("dsym-stage-gone");
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    daemon.debug_bundle_queue().set_sender(tx);
    let withdrawn = daemon.handle_publish_rustc_debug(request(&config, &gone));
    assert!(
        withdrawn
            .error
            .unwrap()
            .contains("taking the staging directory")
    );

    daemon.debug_bundle_queue().close();
    assert!(daemon.debug_bundle_queue().is_closing());
    let after_close = daemon.handle_publish_rustc_debug(request(&config, &staging));
    assert!(after_close.error.unwrap().contains("queue closed"));
}

#[test]
fn the_daemon_refuses_once_the_queue_receiver_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let daemon = Daemon::new(config.clone());
    let staging = handoff_dir(&config).join("dsym-stage-abc");
    std::fs::create_dir_all(&staging).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    daemon.debug_bundle_queue().set_sender(tx);
    let refused = daemon.handle_publish_rustc_debug(request(&config, &staging));
    assert!(refused.error.unwrap().contains("queue closed"));
    assert!(staging.exists());
}

#[test]
fn the_recorded_binary_path_names_the_installed_executable() {
    let from = "/cache/staging/handoff/dsym-job-a/bin/demo";
    let to = "/work/it's/target/debug/deps/demo";
    let text = format!(
        "---\ntriple:          'arm64-apple-darwin'\nbinary-path:     '{from}'\nrelocations:\n  - {{ offset: 0x1 }}\n"
    );
    let rewritten = rewrite_binary_path(&text, from, to).unwrap();
    assert_eq!(
        rewritten,
        "---\ntriple:          'arm64-apple-darwin'\nbinary-path:     '/work/it''s/target/debug/deps/demo'\nrelocations:\n  - { offset: 0x1 }\n"
    );

    let plain = format!("binary-path: {from}\r\nother: x");
    assert_eq!(
        rewrite_binary_path(&plain, from, "/b").unwrap(),
        "binary-path: /b\r\nother: x"
    );
    let quoted_from = "/stage/it's";
    assert_eq!(
        rewrite_binary_path("binary-path: '/stage/it''s'", quoted_from, "/b").unwrap(),
        "binary-path: '/b'"
    );
    assert_eq!(
        rewrite_binary_path("binary-path:     '/elsewhere'\n", from, to),
        None
    );
    assert_eq!(
        rewrite_binary_path(&format!("path: '{from}'\n"), from, to),
        None
    );
}

#[test]
fn relocation_files_in_a_bundle_are_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("demo.dSYM");
    // No relocations directory: nothing to do.
    point_relocations_at(&bundle, Path::new("/a"), Path::new("/b")).unwrap();

    let arch = bundle.join("Contents/Resources/Relocations/aarch64");
    std::fs::create_dir_all(&arch).unwrap();
    std::fs::write(arch.join("demo.yml"), "binary-path:     '/a'\n").unwrap();
    std::fs::write(arch.join("notes.txt"), "binary-path:     '/a'\n").unwrap();
    point_relocations_at(&bundle, Path::new("/a"), Path::new("/b")).unwrap();
    assert_eq!(
        std::fs::read_to_string(arch.join("demo.yml")).unwrap(),
        "binary-path:     '/b'\n"
    );
    assert_eq!(
        std::fs::read_to_string(arch.join("notes.txt")).unwrap(),
        "binary-path:     '/a'\n"
    );
}

fn fake_bundle(dir: &Path, name: &str, dwarf: &[u8]) -> PathBuf {
    let bundle = dir.join(format!("{name}.dSYM"));
    let dwarf_dir = bundle.join("Contents/Resources/DWARF");
    std::fs::create_dir_all(&dwarf_dir).unwrap();
    std::fs::write(dwarf_dir.join(name), dwarf).unwrap();
    std::fs::write(bundle.join("Contents/Info.plist"), b"plist").unwrap();
    bundle
}

#[test]
fn the_bundle_is_placed_next_to_the_executable_it_was_built_from() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let deps = dir.path().join("deps");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::create_dir_all(&deps).unwrap();
    let bundle = fake_bundle(&out, "demo", b"new dwarf");
    let installed = deps.join("demo");
    let dest = deps.join("demo.dSYM");

    // Gone, or not a Mach-O image: nothing placed.
    assert!(!install_beside(&bundle, &installed, &UUID).unwrap());
    std::fs::write(&installed, b"not mach-o").unwrap();
    assert!(!install_beside(&bundle, &installed, &UUID).unwrap());
    // Relinked since: nothing placed.
    std::fs::write(&installed, synthetic_macho(Some(OTHER_UUID), &[])).unwrap();
    assert!(!install_beside(&bundle, &installed, &UUID).unwrap());
    assert!(!dest.exists());

    std::fs::write(&installed, synthetic_macho(Some(UUID), &[])).unwrap();
    assert!(install_beside(&bundle, &installed, &UUID).unwrap());
    assert_eq!(
        std::fs::read(dest.join("Contents/Resources/DWARF/demo")).unwrap(),
        b"new dwarf"
    );
    assert!(
        bundle.exists(),
        "the job's own bundle is still there to tar"
    );

    // A stale bundle is replaced whole: none of its files survive.
    std::fs::write(dest.join("Contents/stale"), b"old").unwrap();
    assert!(install_beside(&bundle, &installed, &UUID).unwrap());
    assert!(!dest.join("Contents/stale").exists());
    assert!(dest.join("Contents/Info.plist").exists());
    let names: Vec<_> = std::fs::read_dir(&deps)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "no staging copies left: {names:?}");
}

#[test]
fn exchanging_into_place_keeps_the_replaced_tree_at_the_staged_path() {
    let dir = tempfile::tempdir().unwrap();
    let staged = dir.path().join("staged");
    let dest = dir.path().join("dest");
    std::fs::create_dir(&staged).unwrap();
    std::fs::write(staged.join("new"), b"n").unwrap();
    exchange_into_place(&staged, &dest).unwrap();
    assert!(dest.join("new").exists());
    assert!(!staged.exists());

    std::fs::create_dir(&staged).unwrap();
    std::fs::write(staged.join("newer"), b"n").unwrap();
    exchange_into_place(&staged, &dest).unwrap();
    assert!(dest.join("newer").exists());
    assert!(!dest.join("new").exists());
    assert!(staged.join("new").exists());
}

/// A stand-in for `dsymutil`: writes a bundle with a relocation file naming
/// the executable it was given, from the directory it runs in.
#[cfg(unix)]
fn fake_dsymutil(executable: &Path, bundle: &Path, objects: &Path) -> Command {
    let script = r#"set -e
mkdir -p "$2/Contents/Resources/DWARF" "$2/Contents/Resources/Relocations/aarch64"
cat deps/a.o > "$2/Contents/Resources/DWARF/demo-1234"
printf "binary-path:     '%s'\n" "$1" > "$2/Contents/Resources/Relocations/aarch64/demo-1234.yml"
"#;
    let mut command = Command::new("sh");
    command
        .args(["-c", script, "dsymutil"])
        .arg(executable)
        .arg(bundle)
        .current_dir(objects);
    command
}

#[cfg(unix)]
fn failing_dsymutil(_: &Path, _: &Path, _: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", "echo 'cannot read' >&2; exit 3"]);
    command
}

/// A job directory as the daemon has it after accepting `fixture`.
fn accepted_job(fixture: &Fixture, config: &Config) -> DebugBundleJob {
    let capture = capture(&handoff_dir(config), &fixture.files(), &fixture.linked()).unwrap();
    let entry = EntryFields {
        cache_key: &key("demo"),
        crate_name: "demo",
        crate_types: &["bin".to_string()],
        features: &[],
        target: "host",
        profile: "dev",
        rustc_identity: Some("1.95.0 (fixture)"),
        unit: Some("1234"),
        stdout: "",
        stderr: "",
        compile_time_ms: 9,
        publishes_to_remote: false,
    };
    let request = request_for(&entry, &fixture.linked(), &capture);
    let uuid = validate(config, &request).unwrap();
    let dir = take_staging(&handoff_dir(config), &capture.dir).unwrap();
    DebugBundleJob { request, dir, uuid }
}

fn fixture_config(fixture: &Fixture) -> Config {
    let cache = fixture.handoff.parent().unwrap().parent().unwrap();
    let mut config = crate::test_support::test_config(cache.to_path_buf());
    config.auto_gc = false;
    config
}

#[cfg(unix)]
#[test]
fn a_job_builds_installs_and_tars_the_bundle() {
    let Some(fixture) = make_fixture(OBJECTS) else {
        return;
    };
    let config = fixture_config(&fixture);
    let job = accepted_job(&fixture, &config);
    // The build rewrites its object after the wrapper returned.
    std::fs::write(fixture.profile.join("deps/a.o"), b"next build").unwrap();

    let files = run_job(&job, &fake_dsymutil).unwrap();
    let names: Vec<_> = files.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["demo-1234", "demo-1234.d", "demo-1234.dsym.tar"]);
    assert!(files.iter().all(|(path, _)| path.starts_with(&job.dir)));

    let installed = fixture
        .profile
        .join("deps/demo-1234.dSYM/Contents/Resources");
    assert_eq!(
        std::fs::read(installed.join("DWARF/demo-1234")).unwrap(),
        b"object a",
        "built from the link-time clone"
    );
    assert_eq!(
        std::fs::read_to_string(installed.join("Relocations/aarch64/demo-1234.yml")).unwrap(),
        format!("binary-path:     '{}'\n", fixture.executable.display())
    );

    let mut archive = tar::Archive::new(std::fs::File::open(&files[2].0).unwrap());
    let mut entries: Vec<_> = archive
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        [
            "Contents/Resources/DWARF/demo-1234",
            "Contents/Resources/Relocations/aarch64/demo-1234.yml"
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_job_whose_dsymutil_fails_yields_no_files() {
    let Some(fixture) = make_fixture(OBJECTS) else {
        return;
    };
    let config = fixture_config(&fixture);
    let job = accepted_job(&fixture, &config);
    let error = run_job(&job, &failing_dsymutil).unwrap_err();
    assert!(format!("{error:#}").contains("cannot read"), "{error:#}");
    assert!(!fixture.profile.join("deps/demo-1234.dSYM").exists());
}

#[cfg(unix)]
#[test]
fn a_processed_job_stores_one_entry_and_removes_its_directory() {
    let Some(fixture) = make_fixture(OBJECTS) else {
        return;
    };
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();

    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &fake_dsymutil),
        JobOutcome::Stored
    );
    assert!(!job_dir.exists());
    let meta = store.get(&key("demo")).unwrap().expect("stored");
    let mut names: Vec<_> = meta.files.iter().map(|file| file.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["demo-1234", "demo-1234.d", "demo-1234.dsym.tar"]);

    // The key is committed now: a second job leaves it to the first.
    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &fake_dsymutil),
        JobOutcome::PeerStored
    );
    assert!(!job_dir.exists());
}

#[cfg(unix)]
#[test]
fn a_failed_or_dropped_job_stores_nothing_and_removes_its_directory() {
    let Some(fixture) = make_fixture(OBJECTS) else {
        return;
    };
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();

    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &failing_dsymutil),
        JobOutcome::Failed
    );
    assert!(!job_dir.exists());
    assert!(store.get(&key("demo")).unwrap().is_none());

    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    daemon.debug_bundle_queue().close();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &fake_dsymutil),
        JobOutcome::Dropped
    );
    assert!(!job_dir.exists());
    assert!(store.get(&key("demo")).unwrap().is_none());
}

#[test]
fn a_claim_waits_for_a_peer_and_never_takes_a_committed_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let store = Store::open(&config).unwrap();
    let peer = Store::open(&config).unwrap();
    let cache_key = key("claimed");

    let BuildClaim::Acquired(held) = peer.claim_build(&cache_key).unwrap() else {
        panic!("fresh key");
    };
    // The peer keeps it past the wait and stores nothing: not ours.
    assert!(
        claim(&store, &cache_key, Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    drop(held);
    // Free again: ours.
    let lock = claim(&store, &cache_key, Duration::from_millis(50)).unwrap();
    assert!(lock.is_some());
}

/// The whole deferred path with the real toolchain: a `-g` C executable
/// linked the way Kache links Rust executables (`-oso_prefix <profile>/`,
/// objects in `deps/`) is captured, its object is then rewritten, and the
/// job still stores the very bundle the synchronous path builds, places a
/// copy next to the executable, and that copy resolves an address to its
/// file and line.
#[test]
fn a_deferred_bundle_matches_the_synchronous_one_and_resolves_lines() {
    use crate::compiler::platform::Platform as _;
    use crate::compiler::platform::tests::{compile_debug_c_binary, relink_c_binary};
    if std::env::consts::OS != "macos" {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    if !clones_files(&root) {
        return;
    }
    let profile = root.join("target/debug");
    let deps = profile.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    let Some(binary) = compile_debug_c_binary(&deps) else {
        return;
    };
    let prefix = format!("-Wl,-oso_prefix,{}/", profile.display());
    if relink_c_binary(&deps.join("hello.o"), &prefix, &binary).is_none() {
        return;
    }

    let sync_staging = tempfile::tempdir().unwrap();
    let sync_tar = crate::compiler::platform::MacOsPlatform::default()
        .package_debug_bundle(&binary, sync_staging.path(), Some("unpacked"))
        .unwrap()
        .expect("the synchronous path packages a bundle");
    let installed_bundle = deps.join("hello-bin.dSYM");
    std::fs::remove_dir_all(&installed_bundle).unwrap();

    let mut config = crate::test_support::test_config(root.join("cache"));
    config.auto_gc = false;
    let files = vec![(binary.clone(), "hello-bin".to_string())];
    let linked = LinkedExecutable {
        path: &binary,
        store_name: "hello-bin",
        oso_root: &profile,
    };
    let capture = capture(&handoff_dir(&config), &files, &linked).unwrap();
    assert!(capture.dir.join("objects/deps/hello.o").is_file());
    // The next build rewrites the object before the job runs.
    std::fs::write(deps.join("hello.o"), b"not an object any more").unwrap();

    let entry = EntryFields {
        cache_key: &key("hello"),
        crate_name: "hello",
        crate_types: &["bin".to_string()],
        features: &[],
        target: "host",
        profile: "dev",
        rustc_identity: None,
        unit: None,
        stdout: "",
        stderr: "",
        compile_time_ms: 1,
        publishes_to_remote: false,
    };
    let request = request_for(&entry, &linked, &capture);
    let uuid = validate(&config, &request).unwrap();
    let dir = take_staging(&handoff_dir(&config), &capture.dir).unwrap();
    let job = DebugBundleJob { request, dir, uuid };

    let deferred_files = run_job(&job, &dsymutil_command).unwrap();
    let deferred_tar = &deferred_files.last().unwrap().0;
    assert_eq!(
        std::fs::read(deferred_tar).unwrap(),
        std::fs::read(&sync_tar).unwrap(),
        "deferred and synchronous bundles are the same bytes"
    );

    let store = Store::open(&config).unwrap();
    let daemon = Arc::new(Daemon::new(config.clone()));
    assert_eq!(
        process_job(&daemon, &config, &store, job, &dsymutil_command),
        JobOutcome::Stored
    );
    let meta = store.get(&key("hello")).unwrap().expect("stored");
    let tar = meta
        .files
        .iter()
        .find(|file| file.name == "hello-bin.dsym.tar")
        .expect("the bundle is part of the entry");
    assert_eq!(tar.hash, crate::cache_key::hash_file(&sync_tar).unwrap());
    assert!(
        std::fs::read_dir(handoff_dir(&config))
            .unwrap()
            .next()
            .is_none(),
        "the job directory is gone"
    );

    // The bundle placed next to the executable resolves `main` to its line.
    let symbols = Command::new("nm").arg(&binary).output().unwrap();
    let symbols = String::from_utf8_lossy(&symbols.stdout);
    let main = symbols
        .lines()
        .find(|line| line.ends_with(" T _main"))
        .and_then(|line| line.split_whitespace().next())
        .expect("nm lists _main");
    let lookup = Command::new("dwarfdump")
        .arg(format!("--lookup=0x{main}"))
        .arg(&installed_bundle)
        .output()
        .unwrap();
    let lookup = String::from_utf8_lossy(&lookup.stdout);
    assert!(
        lookup.contains("hello.c") && lookup.contains("line 2"),
        "{lookup}"
    );
}
