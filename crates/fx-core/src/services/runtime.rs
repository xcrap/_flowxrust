//! Finding the CLIs the providers drive.
//!
//! Neither `codex` nor `claude` is something FlowX bundles: both are the user's
//! own installation, found at launch. That makes this file less about spawning a
//! process and more about deciding whether a file on disk is safe and correct to
//! execute, because FlowX launches by absolute path long after the search.
//!
//! Three decisions are worth stating outright:
//!
//! - **A GUI app cannot trust its own `PATH`.** Launched from Finder, a process
//!   inherits launchd's minimal `PATH`, which omits the usual install roots. So
//!   the search has its own list, and a launch rebuilds `PATH` around the
//!   directory the binary was found in.
//! - **Quarantine is never bypassed.** A binary downloaded from the internet
//!   carries `com.apple.quarantine` until a human clears it. FlowX reports such a
//!   binary and stops; it does not strip the attribute. The one exception is a
//!   signature check, because a quarantined binary that *is* properly notarized
//!   has nothing to clear.
//! - **A resolved runtime is re-checked before every launch.** Discovery runs at
//!   startup and a provider may launch minutes later, by which time the file can
//!   have been replaced, removed, or quarantined.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Directories prepended to `PATH` for every runtime launch.
///
/// A GUI app inherits launchd's minimal `PATH`, which omits the usual install
/// roots of the CLIs and of the interpreters their scripts name in
/// `#!/usr/bin/env`.
pub const PREFERRED_SEARCH_DIRECTORIES: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "$HOME/.local/bin",
];

/// The `PATH` used when the environment has none.
pub const FALLBACK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The attribute macOS puts on a file downloaded from the internet.
///
/// A `CStr`, not a `&str`: it crosses into C, which requires the terminator.
pub const QUARANTINE_ATTRIBUTE: &std::ffi::CStr = c"com.apple.quarantine";

/// A runtime the user installs separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinarySpec {
    pub id: String,
    pub display_name: String,
    /// Where to look, most preferred first. A `*` segment is expanded.
    pub search_paths: Vec<String>,
    /// Arguments that print a version, for the health check.
    pub version_args: Vec<String>,
    /// Arguments always passed to the binary.
    ///
    /// Normally empty, because a spec describes a CLI that takes its own flags.
    /// A test that stands in for a CLI needs them, and rather than adding a
    /// second shape for that, the spec carries them.
    pub arguments: Vec<String>,
    /// A name to ask the shell for, when the direct search finds nothing.
    pub shell_fallback_name: Option<String>,
    /// What to tell the user when it is missing.
    pub install_hint: Option<String>,
}

impl BinarySpec {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        search_paths: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            search_paths,
            version_args: vec!["--version".into()],
            arguments: Vec::new(),
            shell_fallback_name: None,
            install_hint: None,
        }
    }

    pub fn version_args(mut self, args: Vec<String>) -> Self {
        self.version_args = args;
        self
    }

    /// Arguments to pass on every launch of this binary.
    pub fn with_arguments(mut self, arguments: Vec<String>) -> Self {
        self.arguments = arguments;
        self
    }

    pub fn shell_fallback(mut self, name: impl Into<String>) -> Self {
        self.shell_fallback_name = Some(name.into());
        self
    }

    pub fn install_hint(mut self, hint: impl Into<String>) -> Self {
        self.install_hint = Some(hint.into());
        self
    }

    /// The paths a spec searches.
    ///
    /// `RuntimeDiscovery` appends its own directories to this; they go after a
    /// spec's own paths because a spec that names a versioned path means it, and
    /// searching elsewhere first would quietly run a different one.
    pub fn search_paths(&self) -> &[String] {
        &self.search_paths
    }
}

/// What a spec's health is.
///
/// `Quarantined` is a separate case from `NotFound` on purpose: the binary is
/// there and the user can fix it by approving it, which is a different
/// instruction from installing it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BinaryHealth {
    #[default]
    Checking,
    Available {
        path: String,
        version: Option<String>,
    },
    Quarantined {
        path: String,
    },
    NotFound,
}

impl BinaryHealth {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Available { path, .. } | Self::Quarantined { path } => Some(path),
            Self::Checking | Self::NotFound => None,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Available { version, .. } => version.as_deref(),
            _ => None,
        }
    }

    /// One line explaining what to do, for a menu row or a banner.
    pub fn explanation(&self, spec: &BinarySpec) -> String {
        match self {
            Self::Checking => format!("Checking for {}…", spec.display_name),
            Self::Available {
                path: _,
                version: Some(version),
            } => {
                format!("{} {version}", spec.display_name)
            }
            Self::Available { .. } => format!("{} is installed", spec.display_name),
            Self::Quarantined { path } => format!(
                "{} at {path} has not been approved. Open it in Finder and choose Open, then restart FlowX.",
                spec.display_name
            ),
            Self::NotFound => match &spec.install_hint {
                Some(hint) => format!("{} was not found. {hint}", spec.display_name),
                None => format!("{} was not found.", spec.display_name),
            },
        }
    }
}

/// The result of a one-shot runtime command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub status: Option<i32>,
    pub timed_out: bool,
}

impl CommandResult {
    /// Whether the command succeeded.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }

    /// The first line of stdout, for a version string.
    pub fn first_line(&self) -> Option<&str> {
        self.stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    }

    /// Why it failed, for a user-facing message.
    ///
    /// stderr first, because that is where a CLI explains itself; the exit
    /// status is the fallback for a tool that says nothing.
    pub fn failure(&self) -> String {
        if self.timed_out {
            return "the command timed out".to_string();
        }
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        match self.status {
            Some(code) => format!("the command exited with status {code}"),
            None => "the command could not be run".to_string(),
        }
    }
}

/// Everything needed to start a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLaunch {
    pub executable: PathBuf,
    pub environment: BTreeMap<String, String>,
}

/// Whether a resolved binary may be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    /// Resolved and executable.
    Usable(PathBuf),
    /// Executable, but quarantined and not approved.
    Quarantined(PathBuf),
    /// Not there, or not executable.
    Missing,
}

/// Why a runtime could not be launched.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryError {
    #[error("{binary} was not found")]
    NotFound { binary: String },
    #[error("{binary} at {path} has not been approved by the user")]
    Quarantined { binary: String, path: String },
    #[error("{binary} could not be started: {reason}")]
    LaunchFailed { binary: String, reason: String },
}

/// Finds and validates provider runtimes.
#[derive(Clone)]
pub struct RuntimeDiscovery {
    environment: BTreeMap<String, String>,
    /// Extra directories appended to every spec's own paths.
    ///
    /// Configurable rather than a constant so a test can search only where it
    /// put its fixture. Left at the preferred roots in production, where the
    /// whole point is that a GUI app's `PATH` cannot be trusted.
    extra_directories: Vec<String>,
    /// Decides whether a quarantined binary is nevertheless safe.
    ///
    /// A function rather than a flag, so a test can assert the policy without
    /// needing a real signed binary on disk.
    assess_quarantine: QuarantineAssessor,
    /// Reads the quarantine attribute.
    ///
    /// Injectable so a test can describe a quarantined file. Setting the
    /// attribute for real needs `xattr` and only works on macOS, which would make
    /// the policy untestable anywhere else.
    quarantine_probe: QuarantineProbe,
}

/// Reads the quarantine attribute for one path.
///
/// `Some(true)` quarantined, `Some(false)` clean, `None` unreadable.
type QuarantineProbe = Arc<dyn Fn(&Path) -> Option<bool> + Send + Sync>;

/// Decides whether a quarantined binary is nonetheless safe to run.
type QuarantineAssessor = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

impl std::fmt::Debug for RuntimeDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The assessor is a closure, which has no `Debug`; the rest is enough to
        // tell two instances apart in a failure message.
        f.debug_struct("RuntimeDiscovery")
            .field("environment", &self.environment)
            .field("extra_directories", &self.extra_directories)
            .finish_non_exhaustive()
    }
}

impl Default for RuntimeDiscovery {
    fn default() -> Self {
        Self::new(std::env::vars().collect())
    }
}

impl RuntimeDiscovery {
    pub fn new(environment: BTreeMap<String, String>) -> Self {
        Self {
            environment,
            extra_directories: PREFERRED_SEARCH_DIRECTORIES
                .iter()
                .map(|directory| expand_home(directory))
                .collect(),
            assess_quarantine: Arc::new(|_| false),
            quarantine_probe: Arc::new(detect_quarantine),
        }
    }

    /// Replace the quarantine check, for a test that needs a quarantined file.
    pub fn with_quarantine_probe(
        mut self,
        probe: impl Fn(&Path) -> Option<bool> + Send + Sync + 'static,
    ) -> Self {
        self.quarantine_probe = Arc::new(probe);
        self
    }

    /// The directories appended to every spec's own paths.
    pub fn extra_directories(&self) -> &[String] {
        &self.extra_directories
    }

    /// Replace the environment, for a test that needs a particular `PATH`.
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// Search only the paths a spec names.
    ///
    /// Used by tests, which put their fixture in a temp directory and must not
    /// pick up whatever the machine happens to have installed.
    pub fn without_extra_directories(mut self) -> Self {
        self.extra_directories.clear();
        self
    }

    /// Replace the quarantine policy.
    ///
    /// The default refuses everything quarantined. A build that trusts properly
    /// notarized binaries passes an assessor that checks the signature; it never
    /// removes the attribute.
    pub fn with_quarantine_assessor(
        mut self,
        assess: impl Fn(&Path) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.assess_quarantine = Arc::new(assess);
        self
    }

    /// Search for one binary, returning what is there.
    pub async fn find(&self, spec: &BinarySpec) -> Validation {
        let mut quarantined: Option<PathBuf> = None;

        let mut patterns = spec.search_paths().to_vec();
        patterns.extend(self.extra_directories.iter().cloned());
        for pattern in patterns {
            let candidates = if pattern.contains('*') {
                // Newest first: a versioned install should win over an older one
                // that happens to sort earlier.
                let mut expanded = expand_glob(&pattern);
                expanded.sort_by(|a, b| b.cmp(a));
                expanded
            } else {
                vec![PathBuf::from(&pattern)]
            };
            for candidate in candidates {
                match self.validate(&candidate) {
                    Validation::Usable(path) => return Validation::Usable(path),
                    Validation::Quarantined(path) => {
                        quarantined = quarantined.or(Some(path));
                    }
                    Validation::Missing => continue,
                }
            }
        }

        // A shell lookup last: it is the slowest and the least predictable, and
        // it exists for the case where the binary is somewhere unguessable.
        //
        // This blocks, and `find` is reached from `async fn run`, so a login
        // shell sourcing a slow or wedged `~/.zshrc` would occupy a runtime
        // worker for as long as it took. See `shell_which`'s note before changing
        // this to `spawn_blocking` — that needs `find` async, and every caller
        // with it.
        if let Some(path) = shell_lookup(spec).await {
            match self.validate(Path::new(&path)) {
                Validation::Usable(found) => return Validation::Usable(found),
                Validation::Quarantined(found) => quarantined = quarantined.or(Some(found)),
                Validation::Missing => {}
            }
        }

        match quarantined {
            Some(path) => Validation::Quarantined(path),
            None => Validation::Missing,
        }
    }

    /// Search, and report the outcome as a health value.
    pub async fn health(&self, spec: &BinarySpec) -> BinaryHealth {
        match self.find(spec).await {
            Validation::Usable(path) => BinaryHealth::Available {
                path: path.to_string_lossy().into_owned(),
                version: None,
            },
            Validation::Quarantined(path) => BinaryHealth::Quarantined {
                path: path.to_string_lossy().into_owned(),
            },
            Validation::Missing => BinaryHealth::NotFound,
        }
    }

    /// Check whether a path may be executed.
    ///
    /// Indirection is resolved *before* the quarantine check, so a benign symlink
    /// cannot hide a quarantined target behind a clean-looking link.
    pub fn validate(&self, path: &Path) -> Validation {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !is_executable(&resolved) {
            return Validation::Missing;
        }
        match (self.quarantine_probe)(&resolved) {
            Some(false) => Validation::Usable(resolved),
            // Unreadable metadata fails closed: a binary whose attributes cannot
            // be read is one whose provenance cannot be established.
            Some(true) | None => {
                if (self.assess_quarantine)(&resolved) {
                    Validation::Usable(resolved)
                } else {
                    Validation::Quarantined(resolved)
                }
            }
        }
    }

    /// Everything needed to start a runtime.
    ///
    /// Re-validates rather than trusting the search: discovery happens at startup
    /// and a provider may launch much later.
    pub async fn launch(&self, spec: &BinarySpec) -> Result<RuntimeLaunch, DiscoveryError> {
        let path = match self.find(spec).await {
            Validation::Usable(path) => path,
            Validation::Quarantined(path) => {
                return Err(DiscoveryError::Quarantined {
                    binary: spec.display_name.clone(),
                    path: path.to_string_lossy().into_owned(),
                });
            }
            Validation::Missing => {
                return Err(DiscoveryError::NotFound {
                    binary: spec.display_name.clone(),
                });
            }
        };
        Ok(RuntimeLaunch {
            environment: self.launch_environment(path.parent()),
            executable: path,
        })
    }

    /// Run a runtime once and collect its output.
    ///
    /// Used for the checks a provider needs before it can start a session: a
    /// version, a model list, a capability probe.
    pub async fn run(
        &self,
        spec: &BinarySpec,
        arguments: &[String],
        current_directory: Option<&Path>,
        timeout: std::time::Duration,
    ) -> Result<CommandResult, DiscoveryError> {
        let launch = self.launch(spec).await?;
        run_launch(&launch, arguments, current_directory, timeout).await
    }

    /// The environment a runtime launches with.
    ///
    /// The matched binary's own directory goes first, then the preferred install
    /// roots, then whatever the process inherited. Empty entries are dropped,
    /// because an empty `PATH` component means the current directory — which
    /// would let a file in the working directory shadow a real command.
    pub fn launch_environment(&self, search_directory: Option<&Path>) -> BTreeMap<String, String> {
        build_launch_environment(&self.environment, search_directory)
    }
}

/// The `PATH` a launched runtime sees.
pub fn build_launch_environment(
    base: &BTreeMap<String, String>,
    search_directory: Option<&Path>,
) -> BTreeMap<String, String> {
    let inherited = base
        .get("PATH")
        .map(String::as_str)
        .filter(|path| !path.trim().is_empty())
        .unwrap_or(FALLBACK_PATH);

    let mut candidates: Vec<String> = Vec::new();
    if let Some(directory) = search_directory {
        candidates.push(directory.to_string_lossy().into_owned());
    }
    for directory in PREFERRED_SEARCH_DIRECTORIES {
        candidates.push(expand_home(directory));
    }
    candidates.extend(inherited.split(':').map(str::to_string));

    let mut seen = std::collections::HashSet::new();
    let path = candidates
        .into_iter()
        .filter(|entry| !entry.trim().is_empty())
        .filter(|entry| seen.insert(entry.clone()))
        .collect::<Vec<_>>()
        .join(":");

    let mut environment = base.clone();
    environment.insert("PATH".into(), path);
    environment
}

/// Expand a leading `$HOME` in a search directory.
fn expand_home(path: &str) -> String {
    let Some(rest) = path.strip_prefix("$HOME/") else {
        return path.to_string();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };
    PathBuf::from(home)
        .join(rest)
        .to_string_lossy()
        .into_owned()
}

/// Whether a path is a runnable file.
///
/// The execute bit alone is not enough: directories carry it too, and every
/// preferred search directory is one. Checking only that bit made discovery
/// select `/opt/homebrew/bin` as the `codex` binary, and the launch then failed
/// with a permission error on a directory.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    // A symlink to a runnable file must still be rejected; `metadata` follows
    // links, so a dangling one is already gone from here.
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Read the macOS quarantine attribute.
///
/// `None` means the attribute could not be read, which is treated as present:
/// failing closed is the only safe answer when provenance is unknown.
#[cfg(target_os = "macos")]
pub fn detect_quarantine(path: &Path) -> Option<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is NUL-terminated, and the value buffer is NULL with a
    // size of zero, so `getxattr` writes nothing and only reports presence.
    //
    // The attribute name must be a NUL-terminated C string too. Passing
    // `&str::as_ptr()` here reads past the end of the literal while `getxattr`
    // hunts for a terminator, which returns a nonsense errno — and makes every
    // binary look quarantined.
    let result = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            QUARANTINE_ATTRIBUTE.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    match result {
        r if r >= 0 => Some(true),
        _ => {
            // ENOATTR is the only result that proves the file is not quarantined.
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == ENOATTR { Some(false) } else { None }
        }
    }
}

#[cfg(target_os = "macos")]
const ENOATTR: i32 = 93;

/// Expand one `*` in a search pattern.
///
/// Only a trailing `*` on the last component is supported, which is all the
/// specs use. Anything else is returned unchanged rather than guessed at.
pub fn expand_glob(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return Vec::new();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };

    let prefix = name.split('*').next().unwrap_or_default();
    let suffix = name.rsplit('*').next().unwrap_or_default();

    let mut matched: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().to_str()?.to_string();
            if file_name.starts_with(prefix)
                && file_name.ends_with(suffix)
                && file_name.len() >= prefix.len() + suffix.len()
            {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();
    // Newest first, so a versioned install beats an older one.
    matched.sort_by(|a, b| b.cmp(a));
    matched
}

/// Ask the shell where a binary is, on a blocking thread.
///
/// `spawn_blocking` rather than a plain call: this runs a login shell that reads
/// the user's profile, and `find` is reached from `async fn run`. On a runtime
/// worker, a slow or wedged `~/.zshrc` occupies the executor for as long as it
/// takes, stalling every other task queued behind it.
async fn shell_lookup(spec: &BinarySpec) -> Option<String> {
    let name = spec.shell_fallback_name.clone()?;
    tokio::task::spawn_blocking(move || shell_which(&name))
        .await
        .ok()
        .flatten()
}

/// Ask the shell where a binary is.
///
/// The last resort: it runs a login shell, which is slow and reads the user's
/// profile, so it is only reached when a direct search has failed.
fn shell_which(name: &str) -> Option<String> {
    let output = std::process::Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!("command -v {name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// The runtimes FlowX knows about.
pub fn codex_spec() -> BinarySpec {
    BinarySpec::new("codex", "Codex", vec!["/opt/homebrew/bin/codex".into()])
        .version_args(vec!["--version".into()])
        .shell_fallback("codex")
        .install_hint("Install it with `npm install -g @openai/codex`, or see codex.com.")
}

pub fn claude_spec() -> BinarySpec {
    BinarySpec::new(
        "claude",
        "Claude Code",
        vec!["/opt/homebrew/bin/claude".into()],
    )
    .version_args(vec!["--version".into()])
    .shell_fallback("claude")
    .install_hint("Install it with `npm install -g @anthropic-ai/claude-code`.")
}

/// Every runtime FlowX looks for.
pub fn all_specs() -> Vec<BinarySpec> {
    vec![codex_spec(), claude_spec()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A discovery that searches only what the test names.
    ///
    /// Without this, a "not found" assertion can be satisfied by — or defeated
    /// by — whatever CLI happens to be installed on the machine running it.
    use super::tests_support::{environment, hermetic};

    fn executable_at(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[tokio::test]
    async fn the_binary_directory_comes_first_in_a_launch_path() {
        // A provider must be able to invoke the interpreter its own script names,
        // which lives beside the script in an npm install.
        let path = build_launch_environment(
            &environment(&[("PATH", "/usr/bin")]),
            Some(Path::new("/opt/tools/node/bin")),
        );
        let entries: Vec<&str> = path["PATH"].split(':').collect();
        assert_eq!(entries[0], "/opt/tools/node/bin");
        assert!(entries.contains(&"/usr/bin".to_string().as_str()));
    }

    #[tokio::test]
    async fn an_empty_path_component_is_dropped() {
        // An empty component means the working directory, which would let a file
        // there shadow a real command.
        let path = build_launch_environment(&environment(&[("PATH", "")]), None);
        assert!(!path["PATH"].split(':').any(str::is_empty));
    }

    #[tokio::test]
    async fn a_blank_path_falls_back_to_the_system_default() {
        let path = build_launch_environment(&environment(&[("PATH", "   ")]), None);
        assert!(path["PATH"].contains("/usr/bin"));
    }

    #[tokio::test]
    async fn the_preferred_directories_are_present() {
        let path = build_launch_environment(&environment(&[]), None);
        for directory in PREFERRED_SEARCH_DIRECTORIES {
            let expanded = expand_home(directory);
            assert!(
                path["PATH"].split(':').any(|entry| entry == expanded),
                "{expanded} is missing from the launch PATH"
            );
        }
    }

    #[tokio::test]
    async fn the_inherited_environment_is_preserved() {
        let path = build_launch_environment(
            &environment(&[("PATH", "/usr/bin"), ("HOME", "/Users/someone")]),
            None,
        );
        assert_eq!(path["HOME"], "/Users/someone");
    }

    #[tokio::test]
    async fn a_repeated_path_entry_appears_once() {
        // Duplicates in an inherited PATH are common and harmless, but they make
        // every lookup slower and can shadow an intended override.
        let path =
            build_launch_environment(&environment(&[("PATH", "/usr/bin:/usr/bin:/bin")]), None);
        let entries: Vec<&str> = path["PATH"].split(':').collect();
        let mut unique = entries.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            entries.len(),
            unique.len(),
            "a repeated entry survived: {entries:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_as_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let spec = BinarySpec::new(
            "test",
            "Test",
            vec![temp.path().join("absent").to_string_lossy().into()],
        );
        let discovery = hermetic();
        assert_eq!(discovery.health(&spec).await, BinaryHealth::NotFound);
    }

    #[tokio::test]
    async fn an_executable_binary_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("mytool");
        executable_at(&binary);
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let discovery = hermetic();
        match discovery.health(&spec).await {
            BinaryHealth::Available { path, .. } => assert!(path.ends_with("mytool")),
            other => panic!("expected an available binary, got {other:?}"),
        }
        assert!(discovery.health(&spec).await.is_usable());
    }

    #[tokio::test]
    async fn a_file_that_is_not_executable_is_not_found() {
        // A data file with the right name must not be run.
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("mytool");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "not a program").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
            permissions.set_mode(0o644);
            std::fs::set_permissions(&binary, permissions).unwrap();
        }

        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);
        assert_eq!(hermetic().health(&spec).await, BinaryHealth::NotFound);
    }

    #[tokio::test]
    async fn a_directory_is_never_taken_for_a_binary() {
        // Every preferred search directory carries the execute bit, so a check
        // that only tests that bit selects `/opt/homebrew/bin` as the `codex`
        // binary and the launch then fails on a directory.
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("bin");
        std::fs::create_dir_all(&directory).unwrap();
        let spec = BinarySpec::new("test", "Test", vec![directory.to_string_lossy().into()]);

        assert_eq!(hermetic().health(&spec).await, BinaryHealth::NotFound);
    }

    #[tokio::test]
    async fn a_directory_before_a_real_binary_does_not_win() {
        // The search must skip past a directory and find the file behind it.
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("bin");
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("tool");
        executable_at(&binary);
        let spec = BinarySpec::new(
            "test",
            "Test",
            vec![
                directory.to_string_lossy().into_owned(),
                binary.to_string_lossy().into_owned(),
            ],
        );

        match hermetic().find(&spec).await {
            Validation::Usable(path) => assert!(path.ends_with("tool"), "picked {path:?}"),
            other => panic!("expected the binary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_symlink_to_a_directory_is_not_a_binary() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("bin");
        std::fs::create_dir_all(&directory).unwrap();
        let link = temp.path().join("tool");
        std::os::unix::fs::symlink(&directory, &link).unwrap();

        assert_eq!(hermetic().validate(&link), Validation::Missing);
    }

    #[tokio::test]
    async fn a_symlink_is_resolved_before_the_executable_check() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real");
        executable_at(&target);
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let spec = BinarySpec::new("test", "Test", vec![link.to_string_lossy().into()]);
        let discovery = hermetic();
        // The reported path is the target, not the link, so a later launch cannot
        // be redirected by swapping the link.
        match discovery.health(&spec).await {
            BinaryHealth::Available { path, .. } => assert!(path.ends_with("real"), "{path}"),
            other => panic!("expected an available binary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_usable_binary_is_preferred_over_a_quarantined_one() {
        // Real case: a Homebrew cask install of `codex` is quarantined while a
        // separate `~/.local/bin` install is not. Running the working one is
        // better than refusing, and better than silently stripping the
        // quarantine attribute from the other.
        let temp = tempfile::tempdir().unwrap();
        let quarantined = temp.path().join("quarantined");
        let usable = temp.path().join("usable");
        executable_at(&quarantined);
        executable_at(&usable);

        let spec = BinarySpec::new(
            "test",
            "Test",
            vec![
                quarantined.to_string_lossy().into_owned(),
                usable.to_string_lossy().into_owned(),
            ],
        );

        let discovery = RuntimeDiscovery::new(environment(&[]))
            .without_extra_directories()
            .with_quarantine_probe(|path| Some(path.ends_with("quarantined")))
            .with_quarantine_assessor(|_| false);
        match discovery.find(&spec).await {
            Validation::Usable(path) => {
                assert!(path.ends_with("usable"), "picked {path:?}")
            }
            other => panic!("expected the usable binary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_quarantined_binary_is_still_reported_when_it_is_all_there_is() {
        let temp = tempfile::tempdir().unwrap();
        let quarantined = temp.path().join("tool");
        executable_at(&quarantined);
        let spec = BinarySpec::new("test", "Test", vec![quarantined.to_string_lossy().into()]);

        let discovery = RuntimeDiscovery::new(environment(&[]))
            .without_extra_directories()
            .with_quarantine_probe(|_| Some(true))
            .with_quarantine_assessor(|_| false);
        assert!(matches!(
            discovery.find(&spec).await,
            Validation::Quarantined(_)
        ));
    }

    #[tokio::test]
    async fn a_launch_reports_why_a_binary_cannot_be_used() {
        let temp = tempfile::tempdir().unwrap();
        let spec = BinarySpec::new(
            "test",
            "Test",
            vec![temp.path().join("absent").to_string_lossy().into_owned()],
        );
        let discovery = hermetic();
        assert_eq!(
            discovery.launch(&spec).await,
            Err(DiscoveryError::NotFound {
                binary: "Test".into()
            })
        );
    }

    #[tokio::test]
    async fn a_launch_carries_a_path_that_finds_the_providers_own_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("tool");
        executable_at(&binary);
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let launch = hermetic()
            .with_environment(environment(&[("PATH", "/usr/bin")]))
            .launch(&spec)
            .await
            .expect("a launch");
        let resolved = std::fs::canonicalize(&binary).unwrap();
        assert_eq!(launch.executable, resolved);
        // The *resolved* binary's directory, not the matched one: npm keeps
        // `node` beside the resolved script's directory, and it is the resolved
        // one the provider actually execs.
        let directory = resolved.parent().unwrap().to_string_lossy().into_owned();
        assert!(
            launch.environment["PATH"].starts_with(&directory),
            "the binary's own directory is missing: {}",
            launch.environment["PATH"]
        );
    }

    #[tokio::test]
    async fn a_glob_expands_to_matching_files_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["codex-1.0", "codex-2.0", "codex-3.0"] {
            executable_at(&temp.path().join(name));
        }
        std::fs::write(temp.path().join("other-1.0"), b"").unwrap();

        let matched = expand_glob(&temp.path().join("codex-*").to_string_lossy());
        let names: Vec<String> = matched
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["codex-3.0", "codex-2.0", "codex-1.0"]);
    }

    #[tokio::test]
    async fn a_glob_respects_both_a_prefix_and_a_suffix() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["codex-1.0", "codex-1.0-beta", "codex-2.0"] {
            std::fs::write(temp.path().join(name), b"").unwrap();
        }
        let matched = expand_glob(&temp.path().join("codex-*.0").to_string_lossy());
        let names: Vec<String> = matched
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["codex-2.0", "codex-1.0"]);
    }

    #[tokio::test]
    async fn a_glob_over_a_missing_directory_is_empty_rather_than_fatal() {
        assert!(expand_glob("/nonexistent-directory-xyz/tool-*").is_empty());
    }

    #[tokio::test]
    async fn a_glob_matches_a_bare_star() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("anything"), b"").unwrap();
        assert_eq!(
            expand_glob(&temp.path().join("*").to_string_lossy()).len(),
            1
        );
    }

    #[tokio::test]
    async fn a_pattern_without_a_star_expands_to_nothing() {
        // Not an error: the caller falls back to using the path as written.
        assert!(expand_glob("/usr/local/bin/codex").is_empty());
    }

    #[tokio::test]
    async fn the_preferred_directories_are_searched_in_production() {
        // A GUI app inherits launchd's PATH, which omits the usual install
        // roots — so discovery cannot rely on the environment at all.
        let discovery = RuntimeDiscovery::new(environment(&[("PATH", "/usr/bin")]));
        for directory in PREFERRED_SEARCH_DIRECTORIES {
            let expanded = expand_home(directory);
            assert!(
                discovery.extra_directories().contains(&expanded),
                "{expanded} is not searched"
            );
        }
    }

    #[tokio::test]
    async fn a_specs_own_paths_come_first() {
        // A spec that names a versioned path means it; searching the system
        // first would quietly run a different one.
        let spec = BinarySpec::new("test", "Test", vec!["/custom/codex-2".into()]);
        assert_eq!(spec.search_paths(), &["/custom/codex-2".to_string()]);
    }

    #[tokio::test]
    async fn a_missing_binary_explains_how_to_install_it() {
        let spec = codex_spec();
        assert!(
            BinaryHealth::NotFound
                .explanation(&spec)
                .contains("npm install")
        );
    }

    #[tokio::test]
    async fn a_quarantined_binary_explains_how_to_approve_it() {
        // Different instruction from installing it: the binary is already there.
        let spec = codex_spec();
        let explanation = BinaryHealth::Quarantined {
            path: "/tmp/codex".into(),
        }
        .explanation(&spec);
        assert!(explanation.contains("Approve") || explanation.contains("approved"));
        assert!(!explanation.contains("npm install"));
    }

    #[tokio::test]
    async fn a_health_value_reports_its_path_and_version() {
        assert_eq!(
            BinaryHealth::Available {
                path: "/opt/codex".into(),
                version: Some("1.2.3".into())
            }
            .path(),
            Some("/opt/codex")
        );
        assert_eq!(
            BinaryHealth::Available {
                path: "/opt/codex".into(),
                version: Some("1.2.3".into())
            }
            .version(),
            Some("1.2.3")
        );
        assert_eq!(BinaryHealth::NotFound.path(), None);
        assert!(!BinaryHealth::Checking.is_usable());
    }

    #[tokio::test]
    async fn the_known_specs_name_a_shell_fallback_and_a_hint() {
        for spec in all_specs() {
            assert!(
                spec.shell_fallback_name.is_some(),
                "{} has no fallback",
                spec.id
            );
            assert!(spec.install_hint.is_some(), "{} has no hint", spec.id);
            assert_eq!(spec.version_args, vec!["--version".to_string()]);
        }
    }

    #[tokio::test]
    async fn a_home_relative_directory_is_expanded() {
        assert!(!expand_home("$HOME/.local/bin").contains("$HOME"));
    }

    #[tokio::test]
    async fn a_non_home_directory_is_left_alone() {
        assert_eq!(expand_home("/usr/local/bin"), "/usr/local/bin");
    }
}

/// Run one command against an already-resolved launch.
///
/// Split from [`RuntimeDiscovery::run`] so a caller holding a `RuntimeLaunch` —
/// which is what a provider caches — does not have to re-resolve.
pub async fn run_launch(
    launch: &RuntimeLaunch,
    arguments: &[String],
    current_directory: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<CommandResult, DiscoveryError> {
    let mut command = tokio::process::Command::new(&launch.executable);
    command
        .args(arguments)
        .env_clear()
        // `kill_on_drop` so a timeout does not leave an orphan holding a pipe.
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    for (key, value) in &launch.environment {
        command.env(key, value);
    }
    if let Some(directory) = current_directory {
        command.current_dir(directory);
    }

    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        // A runtime that hangs must not hold up the caller forever; a version
        // check is not worth more than a second or two.
        Ok(Err(error)) => {
            return Err(DiscoveryError::LaunchFailed {
                binary: launch.executable.to_string_lossy().into_owned(),
                reason: error.to_string(),
            });
        }
        Err(_) => {
            return Ok(CommandResult {
                timed_out: true,
                ..CommandResult::default()
            });
        }
    };

    Ok(CommandResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status.code(),
        timed_out: false,
    })
}

/// Tests for one-shot commands.
///
/// Separate module because these are async and need a process, unlike the rest
/// of this file.
#[cfg(test)]
mod command_tests {
    use super::tests_support::{environment, hermetic, write_script};
    use super::*;

    #[tokio::test]
    async fn a_one_shot_command_returns_its_output() {
        let temp = tempfile::tempdir().unwrap();
        let binary = write_script(
            &temp.path().join("tool"),
            "echo out-$1\necho err >&2\nexit 0\n",
        );
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let result = hermetic()
            .run(
                &spec,
                &["hello".to_string()],
                None,
                std::time::Duration::from_secs(10),
            )
            .await
            .expect("the command ran");
        assert!(result.succeeded());
        assert_eq!(result.stdout.trim(), "out-hello");
        assert_eq!(result.stderr.trim(), "err");
        assert_eq!(result.first_line(), Some("out-hello"));
    }

    #[tokio::test]
    async fn a_failing_command_explains_itself() {
        let temp = tempfile::tempdir().unwrap();
        let binary = write_script(&temp.path().join("tool"), "echo 'bad flag' >&2\nexit 2\n");
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let result = hermetic()
            .run(&spec, &[], None, std::time::Duration::from_secs(10))
            .await
            .expect("the command ran");
        assert!(!result.succeeded());
        assert_eq!(result.failure(), "bad flag");
        assert_eq!(result.status, Some(2));
    }

    #[tokio::test]
    async fn a_command_that_says_nothing_falls_back_to_its_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        let binary = write_script(&temp.path().join("tool"), "exit 9\n");
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let result = hermetic()
            .run(&spec, &[], None, std::time::Duration::from_secs(10))
            .await
            .expect("the command ran");
        assert!(result.failure().contains('9'), "{}", result.failure());
    }

    #[tokio::test]
    async fn a_command_that_hangs_is_timed_out_rather_than_waited_on() {
        let temp = tempfile::tempdir().unwrap();
        let binary = write_script(&temp.path().join("tool"), "sleep 30\n");
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let result = hermetic()
            .run(&spec, &[], None, std::time::Duration::from_millis(200))
            .await
            .expect("the command ran");
        assert!(result.timed_out);
        assert_eq!(result.failure(), "the command timed out");
    }

    #[tokio::test]
    async fn a_command_finds_its_interpreter_beside_the_binary() {
        // The reason a launch rebuilds PATH: an npm-installed CLI is a script
        // naming `node` in its shebang, and node lives beside it.
        let temp = tempfile::tempdir().unwrap();
        write_script(&temp.path().join("fake-node"), "");
        let binary = write_script(
            &temp.path().join("cli"),
            "if command -v fake-node >/dev/null; then echo found; fi\n",
        );
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        // An empty PATH: what a GUI app launched from Finder actually gets.
        let result = hermetic()
            .with_environment(environment(&[("PATH", "")]))
            .run(&spec, &[], None, std::time::Duration::from_secs(10))
            .await
            .expect("the command ran");
        assert_eq!(
            result.stdout.trim(),
            "found",
            "the sibling interpreter was not on the launch PATH"
        );
    }

    #[tokio::test]
    async fn a_command_runs_in_the_requested_directory() {
        let temp = tempfile::tempdir().unwrap();
        let binary = write_script(&temp.path().join("tool"), "pwd\n");
        let spec = BinarySpec::new("test", "Test", vec![binary.to_string_lossy().into()]);

        let result = hermetic()
            .run(
                &spec,
                &[],
                Some(temp.path()),
                std::time::Duration::from_secs(10),
            )
            .await
            .expect("the command ran");
        let reported = std::fs::canonicalize(result.stdout.trim()).unwrap();
        let expected = std::fs::canonicalize(temp.path()).unwrap();
        assert_eq!(reported, expected);
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_before_the_command_runs() {
        let temp = tempfile::tempdir().unwrap();
        let spec = BinarySpec::new(
            "test",
            "Test",
            vec![temp.path().join("absent").to_string_lossy().into_owned()],
        );
        assert_eq!(
            hermetic()
                .run(&spec, &[], None, std::time::Duration::from_secs(1))
                .await,
            Err(DiscoveryError::NotFound {
                binary: "Test".into()
            })
        );
    }
}

/// Helpers shared by the tests in this file.
#[cfg(test)]
pub mod tests_support {
    use super::*;
    use std::collections::BTreeMap;

    /// A discovery that searches only what a test names.
    ///
    /// Without this, a "not found" assertion can be satisfied by — or defeated
    /// by — whatever CLI happens to be installed on the machine running it.
    pub fn hermetic() -> RuntimeDiscovery {
        RuntimeDiscovery::new(BTreeMap::new()).without_extra_directories()
    }

    pub fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Write an executable shell script, creating its directory.
    #[cfg(unix)]
    pub fn write_script(path: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
        path.to_path_buf()
    }
}
