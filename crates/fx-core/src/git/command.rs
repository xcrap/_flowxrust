//! Running `git` safely.
//!
//! FlowX shells out to `/usr/bin/git` rather than linking a library, which keeps
//! the installed git's own behavior (config, credential helpers, hooks) and
//! avoids a native dependency. The cost is that every invocation needs the same
//! hygiene, so it lives here once.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Where git lives. Absolute so a hostile `PATH` cannot substitute another
/// binary.
pub const GIT: &str = "/usr/bin/git";

/// Default ceiling on a single command's output.
///
/// `git diff` over a large repository can be enormous, but unbounded reads are
/// how a tool quietly eats a machine.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Default ceiling on one command's wall time.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Appended when output was cut short, so the caller can tell the difference
/// between a short diff and a truncated one.
pub const TRUNCATION_NOTICE: &str = "\n\n[Git output truncated by FlowX.]";

/// Why a git invocation did not produce usable output.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not available at {path}")]
    NotInstalled { path: &'static str },
    #[error("git timed out after {0:?}")]
    Timeout(Duration),
    #[error("{command} exited with status {status}{}", detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default())]
    Failed {
        command: String,
        status: i32,
        detail: Option<String>,
    },
    #[error("could not start git: {0}")]
    Spawn(#[source] std::io::Error),
}

/// The result of one git invocation.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
    /// Whether the output hit the byte ceiling and was cut.
    pub truncated: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// How to run one git invocation.
#[derive(Debug, Clone)]
pub struct CommandSpec<'a> {
    pub args: &'a [String],
    pub directory: Option<&'a Path>,
    pub include_stderr_in_stdout: bool,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Suppress the truncation notice, for output the user will never read.
    pub append_truncation_notice: bool,
    /// An index to use in place of the repository's own (`GIT_INDEX_FILE`).
    pub index_file: Option<&'a Path>,
}

impl<'a> CommandSpec<'a> {
    pub fn new(args: &'a [String]) -> Self {
        Self {
            args,
            directory: None,
            include_stderr_in_stdout: false,
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            append_truncation_notice: true,
            index_file: None,
        }
    }

    pub fn in_directory(mut self, directory: Option<&'a Path>) -> Self {
        self.directory = directory;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_index_file(mut self, index_file: Option<&'a Path>) -> Self {
        self.index_file = index_file;
        self
    }

    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }
}

/// The environment every git invocation runs with.
///
/// These settings stop git from blocking on a prompt. FlowX is a background
/// poller: a command that waits for a password is worse than one that fails.
pub fn hardened_environment() -> Vec<(String, String)> {
    vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        // Do not write an index lock during a read-only poll.
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("GCM_INTERACTIVE".into(), "Never".into()),
        ("SSH_ASKPASS_REQUIRE".into(), "never".into()),
        ("GIT_ASKPASS".into(), "/usr/bin/false".into()),
        // Stable collation so status output is byte-identical across machines.
        ("LC_ALL".into(), "C".into()),
    ]
}

/// Run one git command, bounded in both time and output size.
///
/// Exits with [`GitError`] for anything other than a clean exit; callers that
/// treat a non-zero status as ordinary data (a non-repository, say) inspect
/// [`CommandOutput::success`] via [`run_lenient`].
pub async fn run(spec: CommandSpec<'_>) -> Result<CommandOutput, GitError> {
    let command_label = format!("git {}", spec.args.join(" "));
    let output = spawn(spec).await?;
    if !output.success() {
        return Err(GitError::Failed {
            command: command_label,
            status: output.status,
            detail: first_line(&output.stderr).filter(|s| !s.is_empty()),
        });
    }
    Ok(output)
}

/// Run one git command and return its output even on a non-zero exit.
///
/// `git status` in a non-repository is an expected outcome, not an error, and
/// callers need the output to decide what it meant.
pub async fn run_lenient(spec: CommandSpec<'_>) -> Result<CommandOutput, GitError> {
    spawn(spec).await
}

async fn spawn(spec: CommandSpec<'_>) -> Result<CommandOutput, GitError> {
    if !Path::new(GIT).exists() {
        return Err(GitError::NotInstalled { path: GIT });
    }

    let mut command = Command::new(GIT);
    command
        .args(spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .envs(hardened_environment());

    if let Some(dir) = spec.directory {
        command.current_dir(dir);
    }
    if let Some(index) = spec.index_file {
        command.env("GIT_INDEX_FILE", index);
    }

    let mut child = command.spawn().map_err(GitError::Spawn)?;

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // Bound the entire read, not just wait(): a stalled process may keep its
    // pipes open forever without ever reaching wait. Continue draining after
    // the byte cap so a verbose process can finish without blocking on a pipe.
    let collected = tokio::time::timeout(spec.timeout, async {
        let (out, err) = tokio::join!(
            read_capped(&mut stdout, spec.max_output_bytes),
            read_capped(&mut stderr, spec.max_output_bytes),
        );
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((out, err, status.code().unwrap_or(-1)))
    })
    .await;
    let ((mut stdout, truncated), (stderr, _), status) = match collected {
        Ok(Ok(collected)) => collected,
        // `kill_on_drop` makes this actually kill the child.
        Ok(Err(e)) => return Err(GitError::Spawn(e)),
        Err(_) => return Err(GitError::Timeout(spec.timeout)),
    };

    if spec.include_stderr_in_stdout && !stderr.is_empty() {
        if !stdout.is_empty() {
            stdout.push('\n');
        }
        stdout.push_str(&stderr);
    }
    if truncated && spec.append_truncation_notice {
        stdout.push_str(TRUNCATION_NOTICE);
    }

    Ok(CommandOutput {
        stdout,
        stderr,
        status,
        truncated,
    })
}

/// Read a stream up to `cap` bytes, reporting whether the cap was hit.
async fn read_capped<R>(reader: &mut Option<R>, cap: usize) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(reader) = reader.as_mut() else {
        return (String::new(), false);
    };
    let mut buffer = Vec::with_capacity(cap.min(64 * 1024));
    // Keep the buffer off the future's stack: nested status/diff operations
    // otherwise multiply two 64 KiB arrays through every await state.
    let mut chunk = vec![0u8; 64 * 1024];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buffer.len() + n > cap {
                    let room = cap.saturating_sub(buffer.len());
                    buffer.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    continue;
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
        }
    }

    (String::from_utf8_lossy(&buffer).into_owned(), truncated)
}

fn first_line(s: &str) -> Option<String> {
    s.lines().next().map(str::trim).map(str::to_string)
}

/// Content hash for a repository snapshot.
///
/// FNV-1a over everything that determines the displayed state: both status and
/// numstat output, every changed path, each file's size and modification time,
/// and the index's. Including file metadata matters because a file can be
/// rewritten with identical contents — for instance by `git checkout` — and the
/// snapshot would otherwise look unchanged.
///
/// The index is included for the same reason: a `git add` changes what is
/// staged without necessarily changing file bytes on disk.
#[derive(Default)]
pub struct ContentRevisionBuilder {
    hash: u64,
}

const FNV_OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

impl ContentRevisionBuilder {
    pub fn new() -> Self {
        Self {
            hash: FNV_OFFSET_BASIS,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(FNV_PRIME);
        }
    }

    /// Mix in a labeled field. Labels are hashed too, so that reordering two
    /// different fields cannot collide.
    pub fn field(&mut self, label: &str, value: &str) -> &mut Self {
        self.write(label.as_bytes());
        self.write(b"\x1f");
        self.write(value.as_bytes());
        self.write(b"\x1e");
        self
    }

    /// Mix in a file's path, size, and modification time.
    pub fn file(&mut self, path: &Path, size: u64, modified_seconds: i64) -> &mut Self {
        self.field("path", &path.to_string_lossy());
        self.field("size", &size.to_string());
        self.field("mtime", &modified_seconds.to_string());
        self
    }

    /// Mix in everything `stat` says about a path that a rewrite changes.
    ///
    /// Seconds alone miss an agent that rewrites a file twice within one
    /// second at the same size, so the nanoseconds, the inode (an atomic
    /// rename-into-place replaces it) and the change time go in too. A missing
    /// path is recorded as missing rather than skipped, so a file that appears
    /// and one that vanishes cannot hash alike.
    pub fn metadata(&mut self, path: &Path, metadata: Option<&std::fs::Metadata>) -> &mut Self {
        use std::os::unix::fs::MetadataExt;
        let Some(metadata) = metadata else {
            return self.field("missing", &path.to_string_lossy());
        };
        self.file(path, metadata.len(), metadata.mtime());
        self.field("mtime_nsec", &metadata.mtime_nsec().to_string());
        self.field("ctime", &metadata.ctime().to_string());
        self.field("ctime_nsec", &metadata.ctime_nsec().to_string());
        self.field("inode", &metadata.ino().to_string());
        self
    }

    pub fn finish(&self) -> u64 {
        // Fold in the count so a shorter input cannot share a hash with a
        // longer one that happens to end the same way.
        self.hash ^ (self.hash >> 32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_deadline_includes_waiting_for_output() {
        let args = vec!["-c".into(), "alias.pause=!sleep 1".into(), "pause".into()];
        let before = std::time::Instant::now();
        let result =
            run_lenient(CommandSpec::new(&args).with_timeout(Duration::from_millis(30))).await;
        assert!(matches!(result, Err(GitError::Timeout(_))));
        assert!(before.elapsed() < Duration::from_millis(800));
    }

    #[tokio::test]
    async fn output_above_the_cap_is_drained_without_deadlocking() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("large.txt");
        std::fs::write(&file, "a line of text\n".repeat(100_000)).unwrap();
        let args = vec![
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--".into(),
            "/dev/null".into(),
            file.to_string_lossy().into_owned(),
        ];
        let output = run_lenient(
            CommandSpec::new(&args)
                .with_max_output_bytes(1_024)
                .with_timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(
            output.status, 1,
            "a successful diff reports differences with exit 1"
        );
        assert!(output.truncated);
        assert!(output.stdout.len() <= 1_024 + TRUNCATION_NOTICE.len());
    }

    #[test]
    fn the_hardened_environment_suppresses_every_interactive_prompt() {
        let env = hardened_environment();
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(lookup("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        assert_eq!(lookup("GIT_OPTIONAL_LOCKS").as_deref(), Some("0"));
        assert_eq!(lookup("GCM_INTERACTIVE").as_deref(), Some("Never"));
        assert_eq!(lookup("SSH_ASKPASS_REQUIRE").as_deref(), Some("never"));
        assert_eq!(lookup("GIT_ASKPASS").as_deref(), Some("/usr/bin/false"));
        assert_eq!(lookup("LC_ALL").as_deref(), Some("C"));
    }

    #[test]
    fn equal_input_hashes_equally() {
        let build = || {
            let mut b = ContentRevisionBuilder::new();
            b.field("branch", "main").field("status", "## main");
            b.finish()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn any_difference_changes_the_hash() {
        let base = {
            let mut b = ContentRevisionBuilder::new();
            b.field("branch", "main");
            b.finish()
        };
        let other_field = {
            let mut b = ContentRevisionBuilder::new();
            b.field("branch", "develop");
            b.finish()
        };
        let other_order = {
            let mut b = ContentRevisionBuilder::new();
            b.field("status", "## main").field("branch", "main");
            b.finish()
        };
        let other_size = {
            let mut b = ContentRevisionBuilder::new();
            b.field("size", "10").field("branch", "main");
            b.finish()
        };

        for (name, other) in [
            ("value", other_field),
            ("field order", other_order),
            ("field label", other_size),
        ] {
            assert_ne!(base, other, "changing {name} did not change the hash");
        }
    }

    #[test]
    fn a_files_modification_time_participates() {
        let a = {
            let mut b = ContentRevisionBuilder::new();
            b.file(Path::new("a.rs"), 100, 1_700_000_000);
            b.finish()
        };
        let touched = {
            let mut b = ContentRevisionBuilder::new();
            b.file(Path::new("a.rs"), 100, 1_700_000_001);
            b.finish()
        };
        assert_ne!(
            a, touched,
            "a rewritten file with identical contents must change the revision"
        );
    }

    #[test]
    fn an_empty_revision_is_still_a_hash() {
        let empty = ContentRevisionBuilder::new().finish();
        let empty_again = ContentRevisionBuilder::new().finish();
        assert_eq!(empty, empty_again);
    }

    #[test]
    fn the_first_line_of_stderr_is_short() {
        assert_eq!(
            first_line("  fatal: not a git repository  \nmore detail\n").as_deref(),
            Some("fatal: not a git repository")
        );
        assert_eq!(first_line(""), None);
    }
}
