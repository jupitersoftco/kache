//! This fork's default, end to end: with no `macos_debug_bundle` setting, a
//! cached macOS executable's `.dSYM` is built by the daemon after the build
//! returns, stored with the executable, installed next to it, and restored
//! with it on a hit.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;
use common::{DaemonGuard, hermetic_command, kache_binary, settle_writes};

const CRATE: &str = "deferred_default";

struct Project {
    root: PathBuf,
    cache: PathBuf,
    target: PathBuf,
    config: PathBuf,
    tools: PathBuf,
}

impl Project {
    fn new(root: &Path) -> Self {
        let cache = root.join("cache");
        let tools = root.join("tools");
        for dir in [root.join("src"), cache.clone(), tools.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{CRATE}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [workspace]\n\n[profile.dev]\ndebug = 2\nsplit-debuginfo = \"unpacked\"\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"42\"); }\n",
        )
        .unwrap();
        // No `macos_debug_bundle`: the default applies.
        let config = cache.join("config.toml");
        let quoted = toml::Value::String(cache.to_string_lossy().into_owned());
        fs::write(
            &config,
            format!(
                "[cache]\nlocal_only = true\ncache_executables = true\n\
                 adaptive_incremental = false\nlocal_store = {quoted}\nruntime_dir = {quoted}\n"
            ),
        )
        .unwrap();
        // The build's dsymutil, which the daemon runs. The pause keeps the
        // bundle from landing before the build has returned.
        let dsymutil = tools.join("dsymutil");
        fs::write(
            &dsymutil,
            "#!/bin/sh\nsleep 3\nexec /usr/bin/dsymutil \"$@\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dsymutil, fs::Permissions::from_mode(0o755)).unwrap();
        settle_writes(&[root]);
        Self {
            root: root.to_owned(),
            target: root.join("target"),
            cache,
            config,
            tools,
        }
    }

    fn path(&self) -> std::ffi::OsString {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.tools.clone()];
        paths.extend(std::env::split_paths(&inherited));
        std::env::join_paths(paths).unwrap()
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = hermetic_command(program, &self.cache, Some(&self.config));
        // The hermetic environment pins sync for the other fixtures.
        command
            .env_remove("KACHE_MACOS_DEBUG_BUNDLE")
            .env("PATH", self.path());
        command
    }

    fn start_daemon(&self) {
        let output = self
            .command(kache_binary())
            .args(["daemon", "start"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        wait_for("the daemon socket", Duration::from_secs(30), || {
            self.cache.join("daemon.sock").exists()
        });
    }

    fn build(&self) {
        let output = self
            .command(env!("CARGO"))
            .args(["build", "--offline"])
            .current_dir(&self.root)
            .env("RUSTC_WRAPPER", kache_binary())
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_INCREMENTAL", "0")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn events(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.cache.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|event: &serde_json::Value| event["crate_name"] == CRATE)
            .collect()
    }

    /// The linked executable under `deps`.
    fn executable(&self) -> PathBuf {
        fs::read_dir(self.target.join("debug/deps"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("{CRATE}-")))
                    && path.extension().is_none()
                    && path.is_file()
            })
            .expect("the executable under deps")
    }
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {limit:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn bundle_of(executable: &Path) -> PathBuf {
    let mut bundle = executable.as_os_str().to_owned();
    bundle.push(".dSYM");
    PathBuf::from(bundle)
}

fn uuids(path: &Path) -> Vec<String> {
    let output = Command::new("dwarfdump")
        .arg("--uuid")
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_owned))
        .collect()
}

fn assert_matching_bundle(executable: &Path) {
    let bundle = bundle_of(executable);
    assert!(bundle.is_dir(), "{} has no bundle", executable.display());
    let linked = uuids(executable);
    assert!(!linked.is_empty());
    assert_eq!(uuids(&bundle), linked, "the bundle belongs to this link");
}

#[test]
fn by_default_the_daemon_builds_stores_installs_and_restores_the_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let project = Project::new(&root);
    // Declared after the TempDir, so the daemon stops before it is removed.
    let daemon = DaemonGuard::new(kache_binary(), &project.cache);
    project.start_daemon();

    project.build();
    let executable = project.executable();
    assert!(
        !bundle_of(&executable).exists(),
        "the build returned with the bundle already built"
    );
    assert!(
        project.events().is_empty(),
        "the daemon writes the compile's event when its job ends: {:?}",
        project.events()
    );

    wait_for(
        "the daemon's stored entry and installed bundle",
        Duration::from_secs(60),
        || !project.events().is_empty() && bundle_of(&executable).is_dir(),
    );
    let event = project.events().pop().unwrap();
    assert_eq!(event["result"], "miss", "{event}");
    assert_eq!(event["store_handed_off"], true, "{event}");
    // An empty store error is left out of the record.
    assert_eq!(event["store_error"].as_str().unwrap_or(""), "", "{event}");
    assert_matching_bundle(&executable);
    // Cargo's uplifted executable gets its bundle link too.
    assert_matching_bundle(&project.target.join("debug").join(CRATE));

    // A clean build restores executable and bundle from the store.
    fs::remove_dir_all(&project.target).unwrap();
    project.build();
    let event = project.events().pop().unwrap();
    assert_eq!(event["result"], "local_hit", "{event}");
    assert_matching_bundle(&project.executable());

    drop(daemon);
    common::wait_for_daemon_exit(&project.cache, Duration::from_secs(10))
        .expect("the test leaves no daemon running");
}
