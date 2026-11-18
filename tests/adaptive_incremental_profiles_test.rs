//! End-to-end coverage for `[cache] adaptive_incremental_profiles` and for
//! the per-process `KACHE_ADAPTIVE_INCREMENTAL=0` lock, through Cargo and the
//! built `kache` binary.

#![cfg(unix)]

use filetime::FileTime;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// Only the env plumbing is shared; this file drives the Cargo-built binary.
#[allow(dead_code)]
mod common;
use common::hermetic_command;

const CRATE: &str = "profile_fixture";

fn kache_binary() -> &'static str {
    env!("CARGO_BIN_EXE_kache")
}

/// One project, one store and one target, built under a chosen profile and
/// configuration.
struct Fixture {
    // Fields drop in declaration order: the daemon stops while its runtime
    // directory (`cache`) still exists.
    _daemon: common::DaemonGuard,
    project: tempfile::TempDir,
    cache: tempfile::TempDir,
    mtime: i64,
}

impl Fixture {
    /// `settings` are the `[cache]` lines of the config file the builds
    /// read. The file also names this fixture's store and runtime directory,
    /// so a file that pins `ignore_env` never reaches the user's own.
    fn new(settings: &str) -> Self {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let daemon = common::DaemonGuard::new(kache_binary(), cache.path());
        fs::create_dir(project.path().join("src")).unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"profile-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [workspace]\n\n[profile.iteration]\ninherits = \"dev\"\nincremental = true\n",
        )
        .unwrap();
        // Outside the project: a `kache.toml` there declares extra inputs.
        let store = cache.path().display();
        fs::write(
            cache.path().join("config.toml"),
            format!("[cache]\nlocal_store = \"{store}\"\nruntime_dir = \"{store}\"\n{settings}"),
        )
        .unwrap();
        // Past source times keep every write outside the compile's window,
        // so a miss stores; `build` clears Cargo's fingerprint instead.
        let mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 3_600;
        Self {
            _daemon: daemon,
            project,
            cache,
            mtime,
        }
    }

    fn target(&self) -> PathBuf {
        self.project.path().join("target")
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.cache.path().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["crate_name"] == CRATE)
            .collect()
    }

    /// Build source variant `answer` under `profile` and return the one
    /// event the fixture's compile logged. `env` adds to the build's
    /// environment.
    fn build(&mut self, profile: &str, answer: u64, env: &[(&str, &str)]) -> Value {
        let source = self.project.path().join("src/lib.rs");
        fs::write(&source, format!("pub fn answer() -> u64 {{ {answer} }}\n")).unwrap();
        filetime::set_file_mtime(&source, FileTime::from_unix_time(self.mtime, 0)).unwrap();
        self.mtime += 1;
        self.forget_fingerprint(profile);

        let before = self.events().len();
        let mut command = hermetic_command(
            "cargo",
            self.cache.path(),
            Some(&self.cache.path().join("config.toml")),
        );
        command
            .args([
                "build",
                "--offline",
                "--quiet",
                "--lib",
                "--profile",
                profile,
            ])
            .current_dir(self.project.path())
            .env("RUSTC_WRAPPER", kache_binary())
            .env("CARGO_TARGET_DIR", self.target())
            // The fingerprint and policy paths below assume the host layout
            // `target/<profile>` with no separate build directory.
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env("CARGO_INCREMENTAL", "1")
            .env("KACHE_SCHEDULER", "1")
            .env("KACHE_LOG", "kache=debug")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("KACHE_ADAPTIVE_INCREMENTAL")
            .env_remove("KACHE_ADAPTIVE_INCREMENTAL_PROFILES")
            .env_remove("KACHE_CLEAN_INCREMENTAL")
            .env_remove("KACHE_DISABLED")
            .env_remove("KACHE_FALLBACK")
            .env_remove("KACHE_INCREMENTAL_CRATES")
            .env_remove("KACHE_PRESERVE_INCREMENTAL");
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().expect("failed to run cargo");
        assert!(
            output.status.success(),
            "{profile} build of variant {answer} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let events = self.events();
        assert_eq!(
            events.len(),
            before + 1,
            "each variant compiles the fixture exactly once; events:\n{}",
            fs::read_to_string(self.cache.path().join("events.jsonl")).unwrap_or_default(),
        );
        events.into_iter().nth(before).unwrap()
    }

    /// Make Cargo rebuild the fixture under `profile` despite a past source
    /// time.
    fn forget_fingerprint(&self, profile: &str) {
        let dir = if profile == "dev" { "debug" } else { profile };
        let fingerprints = self.target().join(dir).join(".fingerprint");
        for entry in fs::read_dir(&fingerprints).into_iter().flatten() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("profile-fixture-"))
            {
                fs::remove_dir_all(entry.path()).unwrap();
            }
        }
    }

    /// The policy-owned incremental root of a Cargo profile directory.
    fn policy_root(&self, profile_dir: &str) -> PathBuf {
        self.target()
            .join(profile_dir)
            .join("incremental.kache-auto")
    }
}

fn assert_result(event: &Value, result: &str) {
    assert_eq!(event["result"], result, "event: {event:#}");
}

fn assert_adaptive(event: &Value, reason: &str) {
    assert_result(event, "passthrough");
    assert!(
        event["passthrough_reason"]
            .as_str()
            .is_some_and(|value| value.contains(reason)),
        "expected passthrough reason containing {reason:?}; event: {event:#}",
    );
}

/// No unit in this policy root has state or private rustc state.
fn assert_no_adaptive_state(root: &Path) {
    let Ok(versions) = fs::read_dir(root) else {
        return;
    };
    for version in versions {
        for unit in fs::read_dir(version.unwrap().path()).unwrap() {
            let unit = unit.unwrap().path();
            for owned in ["state.json", "rustc"] {
                assert!(
                    !unit.join(owned).exists(),
                    "adaptive state in an out-of-scope profile: {}",
                    unit.join(owned).display()
                );
            }
        }
    }
}

const SCOPED: &str = "adaptive_incremental = true\n\
                      adaptive_incremental_profiles = [\"iteration\"]\n";

#[test]
fn a_unit_outside_the_profile_scope_stays_on_the_cache_path() {
    let mut fixture = Fixture::new(SCOPED);

    // `dev` is out of scope: every edit compiles once and stores, the way it
    // would with adaptive incremental off.
    for answer in 1..=3 {
        let event = fixture.build("dev", answer, &[]);
        assert_result(&event, "miss");
        assert_eq!(event["compiler_runs"], 1, "event: {event:#}");
    }
    assert_no_adaptive_state(&fixture.policy_root("debug"));
    assert_result(&fixture.build("dev", 2, &[]), "local_hit");

    // `iteration` is in scope in the same target and store.
    assert_result(&fixture.build("iteration", 11, &[]), "miss");
    assert_adaptive(&fixture.build("iteration", 12, &[]), "adaptive seed");
    assert_adaptive(&fixture.build("iteration", 13, &[]), "adaptive active");
    assert!(
        fixture.policy_root("iteration").is_dir(),
        "the in-scope profile keeps its policy state"
    );

    // The variable replaces the file's list for one process.
    let debug_only = [("KACHE_ADAPTIVE_INCREMENTAL_PROFILES", "debug")];
    assert_result(&fixture.build("dev", 21, &debug_only), "miss");
    assert_adaptive(&fixture.build("dev", 22, &debug_only), "adaptive seed");
}

/// RedHope's push gate and storer runs set `KACHE_ADAPTIVE_INCREMENTAL=0` for
/// one process while the machine's file turns adaptive on for `iteration`.
/// Those runs must compile, key and store every miss, including under a file
/// that pins `ignore_env`.
#[test]
fn the_per_process_opt_out_stores_in_scope_units_whatever_the_file_says() {
    for config in [SCOPED.to_string(), format!("{SCOPED}ignore_env = true\n")] {
        let mut fixture = Fixture::new(&config);
        let locked = [("KACHE_ADAPTIVE_INCREMENTAL", "0")];
        for answer in 1..=3 {
            let event = fixture.build("iteration", answer, &locked);
            assert_result(&event, "miss");
            assert_eq!(event["compiler_runs"], 1, "{config}\nevent: {event:#}");
        }
        assert_no_adaptive_state(&fixture.policy_root("iteration"));
        // Stored: a return to an earlier variant restores it.
        assert_result(&fixture.build("iteration", 2, &locked), "local_hit");

        // Without the lock the same file adapts.
        assert_result(&fixture.build("iteration", 31, &[]), "miss");
        assert_adaptive(&fixture.build("iteration", 32, &[]), "adaptive seed");
    }
}

/// A storer run can land between two adaptive builds in the same target: a
/// developer's `iteration` unit is adaptive-active, then a run locked with
/// `KACHE_ADAPTIVE_INCREMENTAL=0` compiles new content there (a miss that
/// stores), then the developer builds a third content unlocked. The locked
/// run must key and store normally whatever state the unit holds, and the
/// unlocked build after it must compile the current source.
#[test]
fn a_locked_storer_run_between_adaptive_builds_stores_and_adaptive_resumes() {
    let mut fixture = Fixture::new(SCOPED);
    let locked = [("KACHE_ADAPTIVE_INCREMENTAL", "0")];

    assert_result(&fixture.build("iteration", 1, &[]), "miss");
    assert_adaptive(&fixture.build("iteration", 2, &[]), "adaptive seed");
    assert_adaptive(&fixture.build("iteration", 3, &[]), "adaptive active");

    // The storer run: one keyed compile of the new content.
    let stored = fixture.build("iteration", 4, &locked);
    assert_result(&stored, "miss");
    assert_eq!(stored["compiler_runs"], 1, "event: {stored:#}");

    // The developer's next edit compiles its own source once, either back
    // on the adaptive path or as a keyed miss that restarts learning.
    let resumed = fixture.build("iteration", 5, &[]);
    assert_eq!(resumed["compiler_runs"], 1, "event: {resumed:#}");
    match resumed["result"].as_str() {
        Some("miss") => {}
        Some("passthrough") => assert!(
            resumed["passthrough_reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("adaptive")),
            "event: {resumed:#}"
        ),
        _ => panic!("unexpected result after the storer run; event: {resumed:#}"),
    }

    // What the storer run stored is restorable, and adaptive builds left it
    // intact: a locked return to its content hits.
    assert_result(&fixture.build("iteration", 4, &locked), "local_hit");
}
