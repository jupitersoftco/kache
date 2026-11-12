use crate::BuildEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HandoffFile {
    /// Absolute path of the snapshot, under the store's handoff directory.
    pub path: String,
    /// Name the file takes in the entry.
    pub store_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublishCcRequest {
    /// Client executable mtime. Daemons from 1.0.x and earlier drain when it
    /// is newer than their own; later daemons ignore it.
    #[serde(default)]
    pub client_epoch: u64,
    /// Release requesting an upgrade. Absent from legacy and read-only requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub cache_key: String,
    pub crate_name: String,
    pub target: String,
    pub files: Vec<HandoffFile>,
    pub stdout: String,
    pub stderr: String,
    pub compile_time_ms: u64,
    /// Whether the entry may be uploaded to a writable remote.
    pub publishes_to_remote: bool,
    /// The build event the wrapper would have logged, with the store fields
    /// still empty. The daemon fills them in and writes it.
    pub event: BuildEvent,
    /// The read-set memo of a deferred compile, recorded here instead of in
    /// the wrapper: a hundred-row write transaction the next compile of the
    /// build script would otherwise wait for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<CcMemoHandoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CcMemoHandoff {
    pub memo_key: String,
    pub preprocessed_hash: String,
    pub inputs: Vec<CcPreprocessMemoInput>,
}

/// A compiler input and the filesystem identity observed by its wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcPreprocessMemoInput {
    pub name: String,
    #[serde(flatten)]
    pub fingerprint: FileFingerprint,
    pub content: String,
    #[serde(default)]
    pub mapped: String,
    #[serde(default)]
    pub observed_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: i64,
}

/// A wrapper's request that the daemon build the `.dSYM` of a macOS
/// executable it just linked, then store the executable and that bundle as
/// one entry (`[cache] macos_debug_bundle = "deferred"`).
///
/// Everything the job reads sits in `staging_dir`, a directory the wrapper
/// filled under the store's handoff directory with link-time clones: the
/// executable, every object its debug map names relative to the link's
/// `-oso_prefix` root (under `objects/`), and the entry's other files. The
/// daemon accepts by renaming that directory; the wrapper withdraws by
/// renaming it first. Whichever rename wins owns the job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublishRustcDebugRequest {
    /// Release requesting an upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub cache_key: String,
    pub crate_name: String,
    pub crate_types: Vec<String>,
    pub features: Vec<String>,
    pub target: String,
    pub profile: String,
    /// The `rustc <version>` identity recorded with the entry.
    pub rustc_identity: Option<String>,
    /// `-C metadata` of the unit, recorded with the entry.
    pub unit: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub compile_time_ms: u64,
    /// Whether the entry may be uploaded to a writable remote.
    pub publishes_to_remote: bool,
    /// Absolute path of the wrapper's staging directory.
    pub staging_dir: String,
    /// The entry's files, each a path relative to `staging_dir` with the
    /// name it takes in the entry. The debug bundle is added by the daemon.
    pub files: Vec<HandoffFile>,
    /// The executable among `files`, relative to `staging_dir`.
    pub executable: String,
    /// Entry name of the bundle tar, `<executable>.dsym.tar`.
    pub bundle_store_name: String,
    /// Where the build left the executable. The finished `.dSYM` is placed
    /// next to it if it still has `uuid`.
    pub installed_executable: String,
    /// `LC_UUID` of the executable, as 32 lowercase hex digits.
    pub uuid: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deferred_debug_bundle_request_names_its_release_and_variant() {
        let request = PublishRustcDebugRequest {
            client_version: Some("9.9.9".to_string()),
            cache_key: "k".to_string(),
            crate_name: "demo".to_string(),
            crate_types: vec!["bin".to_string()],
            features: vec![],
            target: "host".to_string(),
            profile: "dev".to_string(),
            rustc_identity: None,
            unit: None,
            stdout: String::new(),
            stderr: String::new(),
            compile_time_ms: 0,
            publishes_to_remote: false,
            staging_dir: "/s".to_string(),
            files: vec![],
            executable: "bin/demo".to_string(),
            bundle_store_name: "demo.dsym.tar".to_string(),
            installed_executable: "/t/demo".to_string(),
            uuid: "0".repeat(32),
        };
        let envelope = crate::Request::PublishRustcDebug(Box::new(request));
        assert_eq!(envelope.client_version(), Some("9.9.9"));
        assert!(envelope.is_build_activity());
        let json = serde_json::to_value(&envelope).unwrap();
        assert!(json.get("publish_rustc_debug").is_some(), "{json}");
        let back: crate::Request = serde_json::from_value(json).unwrap();
        assert_eq!(back, envelope);
    }
}
