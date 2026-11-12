use super::*;
use crate::compiler::macho_debug_map::tests::synthetic_macho;
use crate::config::MacosDebugBundle;

const N_OSO: u8 = 0x66;
const UUID: [u8; 16] = [7; 16];
const OTHER_UUID: [u8; 16] = [9; 16];

fn key(label: &str) -> String {
    blake3::hash(label.as_bytes()).to_hex().to_string()
}

/// What `capture` clones with in these tests: a plain copy, so they run on
/// volumes without copy-on-write too. [`clone_file`] has its own tests.
fn copy_clone(source: &Path, target: &Path) -> Result<()> {
    std::fs::copy(source, target)
        .with_context(|| format!("copying {}", source.display()))
        .map(|_| ())
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
fn only_relative_objects_and_sysroot_rlibs_are_accepted() {
    let sysroot = Some(Path::new("/toolchain"));
    assert_eq!(
        object_to_capture("deps/a.o", sysroot).unwrap(),
        Some(Path::new("deps/a.o"))
    );
    assert_eq!(
        object_to_capture("/toolchain/lib/rustlib/libstd.rlib", sysroot).unwrap(),
        None
    );
    // A static archive a build script wrote, an object the prefix did not
    // strip, and anything absolute when the sysroot is unknown.
    for outside in ["/repo/native/build/libfoo.a", "/target/debug/deps/a.o"] {
        let error = object_to_capture(outside, sysroot).unwrap_err();
        assert!(format!("{error}").contains("sysroot"), "{error}");
    }
    assert!(object_to_capture("/toolchain/lib/libstd.rlib", None).is_err());
    assert!(object_to_capture("/toolchainx/libstd.rlib", sysroot).is_err());
    for escaping in ["../other/a.o", "./a.o", "deps/../../a.o", ""] {
        assert!(
            object_to_capture(escaping, sysroot).is_err(),
            "{escaping:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_sysroot_is_also_matched_through_its_links() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let rlib = real.join("lib/libstd.rlib");
    assert!(in_sysroot(&rlib, &link));
    assert!(in_sysroot(&link.join("lib/libstd.rlib"), &link));
    assert!(!in_sysroot(Path::new("/elsewhere/libstd.rlib"), &link));
}

#[test]
fn copy_on_write_is_the_only_clone() {
    let dir = tempfile::tempdir().unwrap();
    let error = clone_file(&dir.path().join("missing"), &dir.path().join("target")).unwrap_err();
    assert!(format!("{error:#}").contains("copy-on-write"), "{error:#}");
    assert!(!dir.path().join("target").exists());
}

#[test]
fn the_dsymutil_of_a_command_is_its_program_and_environment() {
    let mut command = Command::new("/usr/bin/dsymutil");
    command
        .env_clear()
        .env("DEVELOPER_DIR", "/Applications/Xcode.app")
        .env("PATH", "/usr/bin")
        .env_remove("TOOLCHAINS");
    let dsymutil = Dsymutil::from_command(&command).unwrap();
    assert_eq!(dsymutil.program, "/usr/bin/dsymutil");
    let mut env = dsymutil.env.clone();
    env.sort();
    assert_eq!(
        env,
        [
            (
                "DEVELOPER_DIR".to_string(),
                "/Applications/Xcode.app".to_string()
            ),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_dsymutil_that_cannot_travel_as_text_is_none() {
    use std::os::unix::ffi::OsStrExt as _;
    let odd = std::ffi::OsStr::from_bytes(b"\xff");
    assert_eq!(Dsymutil::from_command(&Command::new(odd)), None);
    let mut command = Command::new("dsymutil");
    command.env("NAME", odd);
    assert_eq!(Dsymutil::from_command(&command), None);
    let mut command = Command::new("dsymutil");
    command.env(odd, "value");
    assert_eq!(Dsymutil::from_command(&command), None);
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
    dsymutil: Dsymutil,
}

fn make_fixture(objects: &[&str]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
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
    Fixture {
        handoff: root.join("cache/staging/handoff"),
        _dir: dir,
        profile,
        executable,
        depinfo,
        dsymutil: Dsymutil {
            program: "dsymutil".to_string(),
            env: vec![("DEVELOPER_DIR".to_string(), "/xcode".to_string())],
        },
    }
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
            sysroot: Some(Path::new("/toolchain")),
            dsymutil: &self.dsymutil,
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
    let fixture = make_fixture(OBJECTS);
    let capture = capture(
        &fixture.handoff,
        &fixture.files(),
        &fixture.linked(),
        &copy_clone,
    )
    .unwrap();

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
        let error = capture(&fixture.handoff, files, &fixture.linked(), &copy_clone).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(
            fixture.staging_dirs().is_empty(),
            "{:?}",
            fixture.staging_dirs()
        );
    };

    let missing = make_fixture(&["deps/missing.o"]);
    failing(&missing, &missing.files(), "missing.o");

    let escaping = make_fixture(&["deps/../escape.o"]);
    failing(&escaping, &escaping.files(), "unexpected OSO path");

    let unprefixed = make_fixture(&[]);
    let inside = unprefixed.profile.join("deps/a.o");
    let symbols = [(N_OSO, inside.to_str().unwrap())];
    std::fs::write(
        &unprefixed.executable,
        synthetic_macho(Some(UUID), &symbols),
    )
    .unwrap();
    failing(&unprefixed, &unprefixed.files(), "sysroot");

    let native = make_fixture(&["/repo/native/build/libfoo.a(foo.o)"]);
    failing(&native, &native.files(), "/repo/native/build/libfoo.a");

    let script = make_fixture(OBJECTS);
    std::fs::write(&script.executable, b"#!/bin/sh\n").unwrap();
    failing(&script, &script.files(), "Mach-O");

    let unlisted = make_fixture(OBJECTS);
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
        lock_key: key("demo shared"),
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
        dsymutil: "dsymutil".to_string(),
        dsymutil_env: vec![("DEVELOPER_DIR".to_string(), "/xcode".to_string())],
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
    let invalid: [(&str, Spoil); 14] = [
        ("key", |r| r.cache_key = "../x".to_string()),
        ("lock key", |r| r.lock_key = "../x".to_string()),
        ("dsymutil", |r| r.dsymutil.clear()),
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
        "binary-path: '/b'\r\nother: x"
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

/// LLVM double-quotes any value with a byte outside ASCII (checked against
/// Apple dsymutil 21: `"/x/ünï \"q\" dir/hé'x"`), and the synchronous path
/// writes the installed path that way.
#[test]
fn non_ascii_paths_are_matched_and_written_double_quoted() {
    let staged = "/cache/staging/handoff/dsym-job-a/bin/hé'x";
    let installed = "/work/ünï \"q\" dir/deps/hé'x";
    let text = "binary-path:     \"/cache/staging/handoff/dsym-job-a/bin/hé'x\"\n";
    assert_eq!(
        rewrite_binary_path(text, staged, installed).unwrap(),
        "binary-path:     \"/work/ünï \\\"q\\\" dir/deps/hé'x\"\n"
    );
    // An ASCII staging path, single-quoted, becomes a double-quoted target.
    assert_eq!(
        rewrite_binary_path("binary-path: '/s/b'\n", "/s/b", "/wörk/b").unwrap(),
        "binary-path: \"/wörk/b\"\n"
    );
}

#[test]
fn values_are_quoted_as_llvm_quotes_them() {
    assert_eq!(yaml_scalar("/a/b"), "'/a/b'");
    assert_eq!(yaml_scalar("/a/it's"), "'/a/it''s'");
    assert_eq!(yaml_scalar("/a\tb"), "'/a\tb'");
    assert_eq!(yaml_scalar("/a\nb"), "'/a\nb'");
    assert_eq!(yaml_scalar("/a\rb"), "'/a\rb'");
    assert_eq!(yaml_scalar("/é"), "\"/é\"");
    assert_eq!(yaml_scalar("/a\x7f"), "\"/a\x7f\"");
    assert_eq!(yaml_scalar("/a\x01"), "\"/a\\x01\"");
    assert_eq!(
        yaml_double_quoted("\\\"\0\x07\x08\t\n\x0b\x0c\r\x1b\x1f\u{85}\u{a0}\u{2028}\u{2029}é"),
        "\"\\\\\\\"\\0\\a\\b\\t\\n\\v\\f\\r\\e\\x1F\\N\\_\\L\\Pé\""
    );
}

#[test]
fn only_old_install_leftovers_of_this_executable_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = ".demo.dSYM.kache-";
    let make = |name: &str, age: Duration| {
        let path = dir.path().join(name);
        std::fs::create_dir(&path).unwrap();
        let time = std::time::SystemTime::now() - age;
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(time)).unwrap();
        path
    };
    let hour = Duration::from_secs(3600);
    let old = make(".demo.dSYM.kache-old", hour * 2);
    let young = make(".demo.dSYM.kache-young", Duration::ZERO);
    let other = make(".other.dSYM.kache-old", hour * 2);
    let file = dir.path().join(".demo.dSYM.kache-file");
    std::fs::write(&file, b"x").unwrap();
    filetime::set_file_mtime(
        &file,
        filetime::FileTime::from_system_time(std::time::SystemTime::now() - hour * 2),
    )
    .unwrap();
    remove_install_leftovers(dir.path(), prefix, hour);
    assert!(!old.exists());
    assert!(young.exists());
    assert!(other.exists());
    assert!(file.exists());
    // A directory that does not exist is no error.
    remove_install_leftovers(&dir.path().join("missing"), prefix, hour);
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
fn uplifted_names_drop_the_metadata_hash() {
    assert_eq!(uplifted_names("kfx-6b24f3748a65f4ab"), ["kfx"]);
    assert_eq!(
        uplifted_names("my_bin-0123456789abcdef"),
        ["my_bin", "my-bin"]
    );
    for none in [
        "kfx",
        "kfx-6b24f3748a65f4a",
        "kfx-6b24f3748a65f4abc",
        "kfx-6b24f3748a65f4ag",
        "-6b24f3748a65f4ab",
    ] {
        assert!(uplifted_names(none).is_empty(), "{none:?}");
    }
}

/// `dir/name` as a synthetic executable with `uuid`.
fn executable_at(dir: &Path, name: &str, uuid: [u8; 16]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, synthetic_macho(Some(uuid), &[])).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn the_uplifted_executable_gets_cargos_bundle_link() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("release");
    let installed = executable_at(&profile.join("deps"), "kfx-6b24f3748a65f4ab", UUID);
    executable_at(&profile, "kfx", UUID);

    let link = uplift_bundle_link(&installed, &UUID).unwrap();
    assert_eq!(link, Some(profile.join("kfx.dSYM")));
    assert_eq!(
        std::fs::read_link(profile.join("kfx.dSYM")).unwrap(),
        Path::new("deps/kfx-6b24f3748a65f4ab.dSYM")
    );
    // An existing link is Cargo's, or already this one: left alone.
    assert_eq!(uplift_bundle_link(&installed, &UUID).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn a_hyphenated_bin_and_an_example_are_linked_where_cargo_links_them() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("debug");
    let installed = executable_at(&profile.join("deps"), "my_bin-0123456789abcdef", UUID);
    executable_at(&profile, "my-bin", UUID);
    assert_eq!(
        uplift_bundle_link(&installed, &UUID).unwrap(),
        Some(profile.join("my-bin.dSYM"))
    );

    let examples = profile.join("examples");
    let example = executable_at(&examples, "demo-0123456789abcdef", UUID);
    executable_at(&examples, "demo", UUID);
    assert_eq!(
        uplift_bundle_link(&example, &UUID).unwrap(),
        Some(examples.join("demo.dSYM"))
    );
    assert_eq!(
        std::fs::read_link(examples.join("demo.dSYM")).unwrap(),
        Path::new("demo-0123456789abcdef.dSYM")
    );
}

#[cfg(unix)]
#[test]
fn only_an_uplifted_copy_of_the_same_link_is_linked() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("release");
    let deps = profile.join("deps");
    let installed = executable_at(&deps, "kfx-6b24f3748a65f4ab", UUID);

    // Not uplifted (a test harness), or no metadata hash: nothing.
    assert_eq!(uplift_bundle_link(&installed, &UUID).unwrap(), None);
    let plain = executable_at(&deps, "kfx", UUID);
    assert_eq!(uplift_bundle_link(&plain, &UUID).unwrap(), None);

    // Uplifted from another link.
    executable_at(&profile, "kfx", OTHER_UUID);
    assert_eq!(uplift_bundle_link(&installed, &UUID).unwrap(), None);
    assert!(std::fs::symlink_metadata(profile.join("kfx.dSYM")).is_err());

    // A link, not a file, in the uplifted executable's place.
    std::fs::remove_file(profile.join("kfx")).unwrap();
    std::os::unix::fs::symlink(&installed, profile.join("kfx")).unwrap();
    assert_eq!(uplift_bundle_link(&installed, &UUID).unwrap(), None);
    assert!(std::fs::symlink_metadata(profile.join("kfx.dSYM")).is_err());
}

#[cfg(unix)]
#[test]
fn a_link_that_cannot_be_made_is_an_error_the_job_only_logs() {
    use std::os::unix::fs::PermissionsExt as _;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes into a read-only directory
    }
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("release");
    let installed = executable_at(&profile.join("deps"), "kfx-6b24f3748a65f4ab", UUID);
    executable_at(&profile, "kfx", UUID);
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = uplift_bundle_link(&installed, &UUID);
    link_uplifted_bundle(&installed, &UUID);
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("kfx.dSYM"), "{error:#}");
    assert!(std::fs::symlink_metadata(profile.join("kfx.dSYM")).is_err());
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
fn fake_dsymutil(
    _: &PublishRustcDebugRequest,
    executable: &Path,
    bundle: &Path,
    objects: &Path,
) -> Command {
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
fn failing_dsymutil(_: &PublishRustcDebugRequest, _: &Path, _: &Path, _: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", "echo 'cannot read' >&2; exit 3"]);
    command
}

#[cfg(unix)]
fn hanging_dsymutil(_: &PublishRustcDebugRequest, _: &Path, _: &Path, _: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 30"]);
    command
}

const NO_TIMEOUT: Duration = Duration::from_secs(60);

fn entry_fields(cache_key: &str) -> EntryFields<'_> {
    EntryFields {
        cache_key,
        lock_key: cache_key,
        crate_name: "demo",
        crate_types: &[],
        features: &[],
        target: "host",
        profile: "dev",
        rustc_identity: Some("1.95.0 (fixture)"),
        unit: Some("1234"),
        stdout: "",
        stderr: "",
        compile_time_ms: 9,
        publishes_to_remote: false,
    }
}

/// A job directory as the daemon has it after accepting `fixture`.
fn accepted_job(fixture: &Fixture, config: &Config) -> DebugBundleJob {
    let capture = capture(
        &handoff_dir(config),
        &fixture.files(),
        &fixture.linked(),
        &copy_clone,
    )
    .unwrap();
    let cache_key = key("demo");
    let request = request_for(&entry_fields(&cache_key), &fixture.linked(), &capture);
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

#[test]
fn the_daemon_runs_the_builds_dsymutil_in_the_builds_environment() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let mut request = request(&config, &handoff_dir(&config).join("dsym-stage-a"));
    request.dsymutil = "/opt/xcode/dsymutil".to_string();
    request.dsymutil_env = vec![("DEVELOPER_DIR".to_string(), "/opt/xcode".to_string())];
    let command = dsymutil_command(
        &request,
        Path::new("/job/bin/demo"),
        Path::new("/job/out/demo.dSYM"),
        Path::new("/job/objects"),
    );
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let tail = ["-j", "2", "/job/bin/demo", "-o", "/job/out/demo.dSYM"];
    assert!(args.ends_with(&tail.map(String::from)), "{args:?}");
    let program = command.get_program().to_string_lossy().into_owned();
    if program == "/usr/sbin/taskpolicy" {
        assert_eq!(args[..3], ["-c", "background", "/opt/xcode/dsymutil"]);
        assert_eq!(args.len(), 3 + tail.len());
    } else {
        assert_eq!(program, "/opt/xcode/dsymutil");
        assert_eq!(args.len(), tail.len());
    }
    let envs: Vec<_> = command.get_envs().collect();
    assert_eq!(
        envs,
        [(
            std::ffi::OsStr::new("DEVELOPER_DIR"),
            Some(std::ffi::OsStr::new("/opt/xcode"))
        )]
    );
    assert_eq!(command.get_current_dir(), Some(Path::new("/job/objects")));
}

#[cfg(unix)]
#[test]
fn a_job_builds_installs_and_tars_the_bundle() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let job = accepted_job(&fixture, &config);
    // The build rewrites its object after the wrapper returned.
    std::fs::write(fixture.profile.join("deps/a.o"), b"next build").unwrap();

    let (files, bundled) = entry_files(&job, &fake_dsymutil, NO_TIMEOUT);
    assert!(bundled);
    let names: Vec<_> = files.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["demo-1234", "demo-1234.d", "demo-1234.dsym.tar"]);
    assert!(files.iter().all(|(path, _)| path.starts_with(&job.dir)));

    let bundle = fixture.profile.join("deps/demo-1234.dSYM");
    let installed = bundle.join("Contents/Resources");
    assert_eq!(
        std::fs::read(installed.join("DWARF/demo-1234")).unwrap(),
        b"object a",
        "built from the link-time clone"
    );
    assert_eq!(
        std::fs::read_to_string(installed.join("Relocations/aarch64/demo-1234.yml")).unwrap(),
        format!("binary-path:     '{}'\n", fixture.executable.display())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&bundle).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

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
fn a_failed_dsymutil_leaves_the_entry_without_a_bundle() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let job = accepted_job(&fixture, &config);
    let error = build_bundle(&job, &failing_dsymutil, NO_TIMEOUT).unwrap_err();
    assert!(format!("{error:#}").contains("cannot read"), "{error:#}");
    let (files, bundled) = entry_files(&job, &failing_dsymutil, NO_TIMEOUT);
    assert!(!bundled);
    let names: Vec<_> = files.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["demo-1234", "demo-1234.d"]);
    assert!(!fixture.profile.join("deps/demo-1234.dSYM").exists());
}

#[cfg(unix)]
#[test]
fn a_hung_dsymutil_is_killed_at_its_timeout() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let job = accepted_job(&fixture, &config);
    let started = std::time::Instant::now();
    let error = build_bundle(&job, &hanging_dsymutil, Duration::from_millis(200)).unwrap_err();
    assert!(format!("{error:#}").contains("was killed"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[cfg(unix)]
#[test]
fn a_processed_job_stores_one_entry_and_removes_its_directory() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();

    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &fake_dsymutil, NO_TIMEOUT),
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
        process_job(&daemon, &config, &store, job, &fake_dsymutil, NO_TIMEOUT),
        JobOutcome::PeerStored
    );
    assert!(!job_dir.exists());
}

#[cfg(unix)]
#[test]
fn a_job_whose_bundle_fails_still_caches_the_executable() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();
    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &failing_dsymutil, NO_TIMEOUT),
        JobOutcome::StoredWithoutBundle
    );
    assert!(!job_dir.exists());
    let meta = store.get(&key("demo")).unwrap().expect("stored");
    let mut names: Vec<_> = meta.files.iter().map(|file| file.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["demo-1234", "demo-1234.d"]);
}

#[cfg(unix)]
#[test]
fn a_dropped_job_stores_nothing_and_removes_its_directory() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();
    let job = accepted_job(&fixture, &config);
    let job_dir = job.dir.clone();
    daemon.debug_bundle_queue().close();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &fake_dsymutil, NO_TIMEOUT),
        JobOutcome::Dropped
    );
    assert!(!job_dir.exists());
    assert!(store.get(&key("demo")).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn a_job_whose_put_fails_stores_nothing() {
    let fixture = make_fixture(OBJECTS);
    let config = fixture_config(&fixture);
    let daemon = Arc::new(Daemon::new(config.clone()));
    let store = Store::open(&config).unwrap();
    let job = accepted_job(&fixture, &config);
    // The entry's executable vanished from the job: the put cannot read it.
    std::fs::remove_file(job.dir.join(&job.request.executable)).unwrap();
    assert_eq!(
        process_job(&daemon, &config, &store, job, &failing_dsymutil, NO_TIMEOUT),
        JobOutcome::Failed
    );
    assert!(store.get(&key("demo")).unwrap().is_none());
}

/// Store a one-file entry under `cache_key`.
fn put_entry(store: &Store, dir: &Path, cache_key: &str) {
    let file = dir.join(format!("{cache_key}.rlib"));
    std::fs::write(&file, cache_key).unwrap();
    store
        .put_rustc_with_compile_time(
            cache_key,
            "peer",
            &[],
            &[],
            "host",
            "dev",
            &[(file, "libpeer.rlib".to_string())],
            "",
            "",
            1,
            None,
        )
        .unwrap();
}

#[test]
fn a_claim_takes_the_wrappers_lock_and_never_a_stored_entry() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let store = Store::open(&config).unwrap();
    let peer = Store::open(&config).unwrap();
    let (shared, width) = (key("shared"), key("width"));
    let wait = Duration::from_millis(50);

    // A peer holds the shared lock past the wait: not ours, whichever entry
    // key the job stores under.
    let held = peer.try_lock(&shared).unwrap().expect("free");
    assert!(claim(&store, &shared, &width, wait).unwrap().is_none());
    drop(held);
    // Free again: ours, and it is the shared key's lock that is held.
    let lock = claim(&store, &shared, &width, wait).unwrap();
    assert!(lock.is_some());
    assert!(peer.try_lock(&shared).unwrap().is_none());
    drop(lock);

    // An entry under the shared key does not stop a width key's job...
    put_entry(&store, dir.path(), &shared);
    assert!(claim(&store, &shared, &width, wait).unwrap().is_some());
    // ...but a stored entry under its own key does, lock free or not.
    put_entry(&store, dir.path(), &width);
    assert!(claim(&store, &shared, &width, wait).unwrap().is_none());
    assert!(claim(&store, &shared, &shared, wait).unwrap().is_none());
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
    use crate::compiler::platform::tests::{
        compile_debug_c_binary, host_tool_unavailable, relink_c_binary,
    };
    if std::env::consts::OS != "macos" {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let profile = root.join("target/debug");
    let deps = profile.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    if compile_debug_c_binary(&deps).is_none() {
        return;
    }
    // Cargo's layout: `deps/<name>-<hash>`, copied up to `<profile>/<name>`.
    let binary = deps.join("hello-0123456789abcdef");
    let prefix = format!("-Wl,-oso_prefix,{}/", profile.display());
    if relink_c_binary(&deps.join("hello.o"), &prefix, &binary).is_none() {
        return;
    }
    let uplifted = profile.join("hello");
    std::fs::copy(&binary, &uplifted).unwrap();

    let sync_staging = tempfile::tempdir().unwrap();
    let sync_tar = crate::compiler::platform::MacOsPlatform::default()
        .package_debug_bundle(&binary, sync_staging.path(), Some("unpacked"))
        .unwrap()
        .expect("the synchronous path packages a bundle");
    let installed_bundle = deps.join("hello-0123456789abcdef.dSYM");
    std::fs::remove_dir_all(&installed_bundle).unwrap();

    let mut config = crate::test_support::test_config(root.join("cache"));
    config.auto_gc = false;
    let files = vec![(binary.clone(), "hello-0123456789abcdef".to_string())];
    let env = crate::invocation::InvocationEnv::capture();
    let dsymutil = Dsymutil::from_command(&env.command("dsymutil").unwrap()).unwrap();
    let linked = LinkedExecutable {
        path: &binary,
        store_name: "hello-0123456789abcdef",
        oso_root: &profile,
        sysroot: None,
        dsymutil: &dsymutil,
    };
    let capture = match capture(&handoff_dir(&config), &files, &linked, &clone_file) {
        Ok(capture) => capture,
        Err(error) => {
            let _: Option<()> = host_tool_unavailable("clonefile", &format!("{error:#}"));
            return;
        }
    };
    assert!(capture.dir.join("objects/deps/hello.o").is_file());
    // The next build rewrites the object before the job runs.
    std::fs::write(deps.join("hello.o"), b"not an object any more").unwrap();

    let cache_key = key("hello");
    let request = request_for(&entry_fields(&cache_key), &linked, &capture);
    let uuid = validate(&config, &request).unwrap();
    let dir = take_staging(&handoff_dir(&config), &capture.dir).unwrap();
    let job = DebugBundleJob { request, dir, uuid };

    let deferred_tar = build_bundle(&job, &dsymutil_command, NO_TIMEOUT).unwrap();
    assert_eq!(
        std::fs::read(&deferred_tar).unwrap(),
        std::fs::read(&sync_tar).unwrap(),
        "deferred and synchronous bundles are the same bytes"
    );

    let store = Store::open(&config).unwrap();
    let daemon = Arc::new(Daemon::new(config.clone()));
    assert_eq!(
        process_job(&daemon, &config, &store, job, &dsymutil_command, NO_TIMEOUT),
        JobOutcome::Stored
    );
    let meta = store.get(&cache_key).unwrap().expect("stored");
    let tar = meta
        .files
        .iter()
        .find(|file| file.name == "hello-0123456789abcdef.dsym.tar")
        .expect("the bundle is part of the entry");
    assert_eq!(tar.hash, crate::cache_key::hash_file(&sync_tar).unwrap());
    assert!(
        std::fs::read_dir(handoff_dir(&config))
            .unwrap()
            .next()
            .is_none(),
        "the job directory is gone"
    );

    // The bundle placed next to the executable, and Cargo's link next to its
    // uplifted copy, resolve `main` to its line.
    let symbols = Command::new("nm").arg(&binary).output().unwrap();
    let symbols = String::from_utf8_lossy(&symbols.stdout);
    let main = symbols
        .lines()
        .find(|line| line.ends_with(" T _main"))
        .and_then(|line| line.split_whitespace().next())
        .expect("nm lists _main");
    let uplifted_bundle = profile.join("hello.dSYM");
    assert_eq!(
        std::fs::read_link(&uplifted_bundle).unwrap(),
        Path::new("deps/hello-0123456789abcdef.dSYM")
    );
    for bundle in [&installed_bundle, &uplifted_bundle] {
        let lookup = Command::new("dwarfdump")
            .arg(format!("--lookup=0x{main}"))
            .arg(bundle)
            .output()
            .unwrap();
        let lookup = String::from_utf8_lossy(&lookup.stdout);
        assert!(
            lookup.contains("hello.c") && lookup.contains("line 2"),
            "{}: {lookup}",
            bundle.display()
        );
    }
}
