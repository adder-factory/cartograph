//! The supervised `cartograph index --supervised` child of `upgrade --apply`.
//!
//! The child reports one compact JSON progress line on stderr whenever its
//! observable work changes, from its start: while it connects and migrates
//! the schema, a bounded startup liveness line changes instead. It is
//! stopped only on an explicit bound:
//! [`IndexChildPolicy::inactivity`] without a changed progress line, or the
//! [`IndexChildPolicy::ceiling`] since spawn. Stopping first closes the
//! child's stdin, which it treats as a cooperative cancellation request (the
//! indexer fails its staging generation and releases its exact project
//! lease), and only after [`IndexChildPolicy::grace`] falls back to a hard
//! kill, whose lease then expires on its own TTL.

use std::{process::Stdio, time::Duration};

use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader},
    process::{Child, ChildStdin, Command},
    time::Instant,
};

use crate::supervised_index::{SUPERVISED_STARTUP_ALLOWANCE, SUPERVISED_WRITER_WAIT};

/// Longest the index child may go without a changed progress line. The
/// child's own supervisor stops a stage after 10 minutes without progress
/// with a precise `*_progress_stalled` code, so this backstop fires only when
/// the child stops reporting altogether, including a startup that outlived
/// its liveness allowance; the margin also absorbs delayed lease heartbeats
/// and unreported preparation such as the source scan.
pub(super) const INDEX_INACTIVITY_TIMEOUT: Duration = Duration::from_mins(15);
/// Time for one generation build: the child's 2-hour supervisor operation
/// budget plus source scans, Git history, and retention around it.
const INDEX_BUILD_ALLOWANCE: Duration = Duration::from_mins(150);
/// Absolute bound on the index child even while it keeps reporting progress:
/// its startup (connection and schema migrations), its bounded wait for a
/// competing writer, and one generation build.
pub(super) const INDEX_ABSOLUTE_CEILING: Duration = SUPERVISED_STARTUP_ALLOWANCE
    .saturating_add(SUPERVISED_WRITER_WAIT)
    .saturating_add(INDEX_BUILD_ALLOWANCE);
/// Time a cooperatively cancelled child gets to fail its generation and
/// release its lease before it is killed. It covers the supervisor's own
/// finish reserve (10 s worker grace, a 3-minute COPY, five 5-second database
/// steps) and stays below the 5-minute lease TTL a killed child would leave.
pub(super) const INDEX_TERMINATION_GRACE: Duration = Duration::from_mins(4);
/// How long pipes are drained after the child exits, in case a descendant
/// inherited them.
const PIPE_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(5);
/// How long a killed child is awaited before supervision gives up on it.
const KILLED_CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(30);
/// The child's primary failure code for a cooperative cancellation. Its
/// cleanup is confirmed only when the failure carries no `cleanup_failure`.
const CANCELLATION_CODE: &str = "request_cancelled";
/// Bound on the final index report read from stdout.
const MAXIMUM_INDEX_REPORT_BYTES: usize = 8 * 1024 * 1024;
/// Bound on one stderr line; longer lines are truncated and never progress.
const MAXIMUM_STDERR_LINE_BYTES: usize = 64 * 1024;
/// Bound on retained non-progress stderr, which ends with the failure JSON.
const MAXIMUM_STDERR_TAIL_BYTES: usize = 64 * 1024;
/// Bound on a stable machine-readable failure code accepted from the child.
const MAXIMUM_FAILURE_CODE_BYTES: usize = 64;
/// Bound on a generation or source-revision identity accepted from the child.
const MAXIMUM_IDENTITY_BYTES: usize = 256;

/// Explicit bounds for one supervised index child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IndexChildPolicy {
    /// Longest interval without a changed progress line.
    pub(super) inactivity: Duration,
    /// Longest total run, even while progress continues.
    pub(super) ceiling: Duration,
    /// Time between the cooperative stop request and a hard kill.
    pub(super) grace: Duration,
}

/// Production bounds.
pub(super) const DEFAULT_INDEX_CHILD_POLICY: IndexChildPolicy = IndexChildPolicy {
    inactivity: INDEX_INACTIVITY_TIMEOUT,
    ceiling: INDEX_ABSOLUTE_CEILING,
    grace: INDEX_TERMINATION_GRACE,
};

/// What the supervised index child concluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IndexChildOutcome {
    /// Exit 0 with a decodable index report.
    Completed(IndexChildReport),
    /// The child failed on its own; `code` is its stable failure code.
    Failed {
        /// Validated `error.code` from the child's failure JSON.
        code: Option<String>,
    },
    /// A parent bound expired and the child was stopped.
    TimedOut(IndexChildTimeout),
}

/// The fields of a successful index report the upgrade relies on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IndexChildReport {
    /// Generation the request published or confirmed as current.
    pub(super) generation_id: String,
    /// Source manifest that generation was built from.
    pub(super) source_revision: String,
    /// The checkout changed after this request published.
    pub(super) changed_after_publication: bool,
}

/// Which bound stopped the child and how it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IndexChildTimeout {
    /// The expired bound.
    pub(super) trigger: DeadlineTrigger,
    /// Cooperative exit within the grace, or a hard kill after it.
    pub(super) stop: ChildStop,
}

/// The parent bound that expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeadlineTrigger {
    /// No changed progress line within the inactivity timeout.
    NoProgress,
    /// The absolute ceiling elapsed.
    Ceiling,
}

impl DeadlineTrigger {
    /// Stable `projectReconciliation.index.reason` value.
    pub(super) const fn reason(self) -> &'static str {
        match self {
            Self::NoProgress => "no_progress",
            Self::Ceiling => "ceiling",
        }
    }
}

/// How a timed-out child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChildStop {
    /// It exited within the grace with `request_cancelled` and no
    /// `cleanup_failure`, which it reports only after PostgreSQL showed its
    /// lease released and no generation it reserved still `staging` or
    /// `ready`.
    Cooperative,
    /// It exited within the grace with another failure, or with
    /// `request_cancelled` plus a `cleanup_failure`, so the cleanup is not
    /// confirmed.
    Unconfirmed,
    /// It did not exit within the grace and was killed; its lease expires
    /// on its own TTL.
    Forced,
}

/// Run `command` as a supervised index child under `policy`.
pub(super) async fn run_index_child(
    mut command: Command,
    policy: IndexChildPolicy,
) -> IndexChildOutcome {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return IndexChildOutcome::Failed { code: None };
    };
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill().await;
        return IndexChildOutcome::Failed { code: None };
    };
    let mut supervision = Supervision {
        child,
        deadlines: Deadlines::new(policy, stdin),
        stderr: StderrScan::new(stderr),
        exit: None,
    };
    let report = supervision.supervise(stdout).await;
    supervision.conclude(report.as_deref())
}

struct Supervision<Stderr> {
    child: Child,
    deadlines: Deadlines,
    stderr: StderrScan<Stderr>,
    exit: Option<bool>,
}

impl<Stderr: AsyncRead + Unpin> Supervision<Stderr> {
    /// Drive the child to exit while reading both pipes; returns stdout.
    async fn supervise(&mut self, stdout: impl AsyncRead + Unpin) -> Option<Vec<u8>> {
        let report = read_bounded(stdout, MAXIMUM_INDEX_REPORT_BYTES);
        tokio::pin!(report);
        let mut report_bytes = None;
        let mut stderr_open = true;
        loop {
            if self.exit.is_some() && report_bytes.is_some() && !stderr_open {
                return report_bytes.flatten();
            }
            let wake = self.deadlines.next_wake(self.exit.is_some());
            tokio::select! {
                status = self.child.wait(), if self.exit.is_none() => {
                    self.exit = Some(status.is_ok_and(|status| status.success()));
                    self.deadlines.child_exited();
                }
                bytes = &mut report, if report_bytes.is_none() => report_bytes = Some(bytes),
                line = self.stderr.next_line(), if stderr_open => {
                    stderr_open = self.observe_stderr(line);
                }
                () = tokio::time::sleep_until(wake) => {
                    if self.exit.is_some() || !self.deadlines.expire(&mut self.child) {
                        return report_bytes.flatten();
                    }
                }
            }
        }
    }

    /// Record one stderr line; false at EOF or on a read failure.
    fn observe_stderr(&mut self, line: std::io::Result<Option<Vec<u8>>>) -> bool {
        let Ok(Some(line)) = line else {
            return false;
        };
        if self.stderr.classify(&line) {
            self.deadlines.progressed();
        }
        true
    }

    fn conclude(&self, report: Option<&[u8]>) -> IndexChildOutcome {
        let succeeded = self.exit == Some(true);
        if succeeded && let Some(report) = report.and_then(decode_index_report) {
            return IndexChildOutcome::Completed(report);
        }
        let failure = (!succeeded).then(|| self.stderr.final_failure()).flatten();
        let Some(trigger) = self.deadlines.expired else {
            return IndexChildOutcome::Failed {
                code: failure.map(|failure| failure.code),
            };
        };
        let stop = if self.deadlines.killed_at.is_some() {
            ChildStop::Forced
        } else if failure
            .as_ref()
            .is_some_and(ChildFailure::confirms_cancellation)
        {
            ChildStop::Cooperative
        } else {
            ChildStop::Unconfirmed
        };
        IndexChildOutcome::TimedOut(IndexChildTimeout { trigger, stop })
    }
}

/// Inactivity, ceiling, grace, post-kill, and post-exit drain bookkeeping.
struct Deadlines {
    policy: IndexChildPolicy,
    ceiling: Instant,
    last_progress: Instant,
    stdin: Option<ChildStdin>,
    expired: Option<DeadlineTrigger>,
    grace_ends: Option<Instant>,
    killed_at: Option<Instant>,
    drain_ends: Option<Instant>,
}

impl Deadlines {
    fn new(policy: IndexChildPolicy, stdin: ChildStdin) -> Self {
        let now = Instant::now();
        Self {
            policy,
            ceiling: now + policy.ceiling,
            last_progress: now,
            stdin: Some(stdin),
            expired: None,
            grace_ends: None,
            killed_at: None,
            drain_ends: None,
        }
    }

    fn progressed(&mut self) {
        self.last_progress = Instant::now();
    }

    fn child_exited(&mut self) {
        self.drain_ends = Some(Instant::now() + PIPE_DRAIN_AFTER_EXIT);
    }

    /// The next instant at which [`Self::expire`] (or the drain) must act.
    fn next_wake(&self, exited: bool) -> Instant {
        if exited {
            return self.drain_ends.unwrap_or_else(Instant::now);
        }
        match (self.killed_at, self.grace_ends) {
            (Some(killed_at), _) => killed_at + KILLED_CHILD_REAP_TIMEOUT,
            (None, Some(grace_ends)) => grace_ends,
            (None, None) => self
                .ceiling
                .min(self.last_progress + self.policy.inactivity),
        }
    }

    /// Escalate one step: request a cooperative stop, then kill after the
    /// grace, then stop waiting for a killed child that never exits. Returns
    /// false once supervision should give up on the child.
    fn expire(&mut self, child: &mut Child) -> bool {
        let now = Instant::now();
        if self.killed_at.is_some() {
            return false;
        }
        if self.grace_ends.is_some() {
            self.killed_at = Some(now);
            let _ = child.start_kill();
            return true;
        }
        self.expired = Some(if now >= self.ceiling {
            DeadlineTrigger::Ceiling
        } else {
            DeadlineTrigger::NoProgress
        });
        // Closing stdin is the child's cooperative cancellation request.
        drop(self.stdin.take());
        self.grace_ends = Some(now + self.policy.grace);
        true
    }
}

/// Bounded line reader over the child's stderr.
struct StderrScan<Stderr> {
    reader: BufReader<Stderr>,
    line: Vec<u8>,
    last_progress: Option<serde_json::Value>,
    tail: Vec<u8>,
}

impl<Stderr: AsyncRead + Unpin> StderrScan<Stderr> {
    fn new(stderr: Stderr) -> Self {
        Self {
            reader: BufReader::new(stderr),
            line: Vec::new(),
            last_progress: None,
            tail: Vec::new(),
        }
    }

    /// Read one line of at most [`MAXIMUM_STDERR_LINE_BYTES`] bytes, or the
    /// unterminated remainder at EOF. Partial lines live in `self`, so the
    /// future may be dropped and polled again without losing bytes.
    async fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok((!self.line.is_empty()).then(|| std::mem::take(&mut self.line)));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let taken = newline.map_or(available.len(), |position| position + 1);
            let room = MAXIMUM_STDERR_LINE_BYTES.saturating_sub(self.line.len());
            let content = newline.unwrap_or(available.len());
            self.line.extend_from_slice(&available[..content.min(room)]);
            self.reader.consume(taken);
            if newline.is_some() {
                return Ok(Some(std::mem::take(&mut self.line)));
            }
        }
    }
}

impl<Stderr> StderrScan<Stderr> {
    /// True when `line` is a progress line that differs from the previous one.
    /// Every other line joins the bounded tail that holds the failure JSON.
    fn classify(&mut self, line: &[u8]) -> bool {
        if let Ok(ProgressLine { progress }) = serde_json::from_slice::<ProgressLine>(line) {
            let changed = self.last_progress.as_ref() != Some(&progress);
            self.last_progress = Some(progress);
            return changed;
        }
        self.tail.extend_from_slice(line);
        self.tail.push(b'\n');
        let excess = self.tail.len().saturating_sub(MAXIMUM_STDERR_TAIL_BYTES);
        self.tail.drain(..excess);
        false
    }

    /// The child's final failure JSON, which is the last document that
    /// starts in column zero of the retained tail, with a validated code.
    fn final_failure(&self) -> Option<ChildFailure> {
        let start = self
            .tail
            .array_windows()
            .rposition(|pair| pair == b"\n{")
            .map_or(0, |position| position + 1);
        let failure: FailureLine = serde_json::from_slice(&self.tail[start..]).ok()?;
        let FailureFields {
            code,
            cleanup_failure,
        } = failure.error;
        (!code.is_empty()
            && code.len() <= MAXIMUM_FAILURE_CODE_BYTES
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
        .then_some(ChildFailure {
            code,
            cleanup_failed: cleanup_failure.is_some(),
        })
    }
}

/// The child's validated failure report.
struct ChildFailure {
    /// Stable primary failure code.
    code: String,
    /// The report carried a secondary `cleanup_failure`.
    cleanup_failed: bool,
}

impl ChildFailure {
    /// A cooperative cancellation whose cleanup the child confirmed.
    fn confirms_cancellation(&self) -> bool {
        self.code == CANCELLATION_CODE && !self.cleanup_failed
    }
}

#[derive(Deserialize)]
struct ProgressLine {
    progress: serde_json::Value,
}

#[derive(Deserialize)]
struct FailureLine {
    error: FailureFields,
}

/// The failure fields supervision relies on; every other field, such as a
/// `previous_generation_visible` that may be `null`, is ignored.
#[derive(Deserialize)]
struct FailureFields {
    code: String,
    /// Present (as an object) only when cleanup of what the child held also
    /// failed or could not be confirmed; its contents are not needed.
    #[serde(default)]
    cleanup_failure: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct IndexReportFields {
    generation_id: String,
    source_revision: String,
    #[serde(default)]
    live_source: Option<String>,
}

fn decode_index_report(bytes: &[u8]) -> Option<IndexChildReport> {
    let fields: IndexReportFields = serde_json::from_slice(bytes).ok()?;
    let bounded = |value: &str| !value.is_empty() && value.len() <= MAXIMUM_IDENTITY_BYTES;
    if !bounded(&fields.generation_id) || !bounded(&fields.source_revision) {
        return None;
    }
    // `unverified` publications are judged by the next-process status probe,
    // exactly like `matched` ones; only an observed change is reported here.
    let changed_after_publication = match fields.live_source.as_deref() {
        None | Some("matched" | "unverified") => false,
        Some("changed_after_publication") => true,
        Some(_) => return None,
    };
    Some(IndexChildReport {
        generation_id: fields.generation_id,
        source_revision: fields.source_revision,
        changed_after_publication,
    })
}

/// Read at most `maximum` bytes; `None` when the stream is longer or fails.
/// An oversized stream is still drained to its end, so a child writing more
/// than the bound can exit instead of blocking on a full pipe until a
/// supervision deadline stops it.
async fn read_bounded(mut stream: impl AsyncRead + Unpin, maximum: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(maximum).ok()?.saturating_add(1);
    (&mut stream)
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() <= maximum {
        return Some(bytes);
    }
    let _discarded = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    None
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

    use super::*;

    /// Bounds small enough for a test but with a grace far above any
    /// cooperative exit, so a cooperative outcome cannot be a slow kill.
    const TEST_POLICY: IndexChildPolicy = IndexChildPolicy {
        inactivity: Duration::from_millis(600),
        ceiling: Duration::from_secs(60),
        grace: Duration::from_secs(20),
    };

    /// A failure document in the child's pretty-printed shape, including a
    /// visibility lookup that failed (`null`).
    const CANCELLED_FAILURE: &str = r#"printf '{\n  "error": {\n    "code": "request_cancelled",\n    "message": "cancelled",\n    "previous_generation_visible": null\n  }\n}\n' >&2"#;

    fn fixture_child(directory: &Path, body: &str) -> Command {
        let script = directory.join("fixture-index");
        fs::write(&script, format!("#!/bin/sh\n{body}\n"))
            .unwrap_or_else(|error| panic!("fixture child write failed: {error}"));
        let mut permissions = fs::metadata(&script)
            .unwrap_or_else(|error| panic!("fixture child metadata failed: {error}"))
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions)
            .unwrap_or_else(|error| panic!("fixture child chmod failed: {error}"));
        let mut command = Command::new(script);
        command.current_dir(directory).kill_on_drop(true);
        command
    }

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"))
    }

    #[tokio::test]
    async fn a_stalled_child_is_asked_to_stop_and_releases_cooperatively_before_any_kill() {
        let directory = workspace();
        // The child reports once, then blocks on stdin like a real supervised
        // index; only the cooperative EOF lets it run its cleanup.
        let child = fixture_child(
            directory.path(),
            &format!(
                "printf '%s\\n' '{{\"progress\":{{\"completed_items\":1}}}}' >&2\ncat > /dev/null\n: > released\n{CANCELLED_FAILURE}\nexit 1"
            ),
        );
        let started = std::time::Instant::now();
        let outcome = run_index_child(child, TEST_POLICY).await;
        assert_eq!(
            outcome,
            IndexChildOutcome::TimedOut(IndexChildTimeout {
                trigger: DeadlineTrigger::NoProgress,
                stop: ChildStop::Cooperative,
            })
        );
        assert!(
            directory.path().join("released").exists(),
            "the child must get to run its cleanup instead of being killed"
        );
        assert!(started.elapsed() < TEST_POLICY.grace);
    }

    #[tokio::test]
    async fn changing_progress_keeps_a_long_index_alive_past_the_inactivity_window() {
        let directory = workspace();
        // Runs for roughly four inactivity windows, reporting new work every
        // 100 ms, and honors a stop request like the real child; a fixed
        // wall-clock limit of one window would stop it.
        let child = fixture_child(
            directory.path(),
            &format!(
                "exec 3<&0\n( cat <&3 > /dev/null; : > stop-requested ) > /dev/null 2>&1 &\ni=0\nwhile [ $i -lt 25 ]; do\n  if [ -e stop-requested ]; then\n    {CANCELLED_FAILURE}\n    exit 1\n  fi\n  printf '{{\"progress\":{{\"completed_items\":%d}}}}\\n' \"$i\" >&2\n  i=$((i+1))\n  sleep 0.1\ndone\nprintf '%s\\n' '{{\"generation_id\":\"g-1\",\"source_revision\":\"r-1\",\"live_source\":\"matched\"}}'"
            ),
        );
        let outcome = run_index_child(child, TEST_POLICY).await;
        assert_eq!(
            outcome,
            IndexChildOutcome::Completed(IndexChildReport {
                generation_id: "g-1".to_owned(),
                source_revision: "r-1".to_owned(),
                changed_after_publication: false,
            })
        );
    }

    #[tokio::test]
    async fn repeated_identical_progress_is_not_progress() {
        let directory = workspace();
        let child = fixture_child(
            directory.path(),
            &format!(
                "( while :; do printf '%s\\n' '{{\"progress\":{{\"completed_items\":7}}}}' >&2; sleep 0.05; done ) &\nticker=$!\ncat > /dev/null\nkill $ticker\n{CANCELLED_FAILURE}\nexit 1"
            ),
        );
        let outcome = run_index_child(child, TEST_POLICY).await;
        assert_eq!(
            outcome,
            IndexChildOutcome::TimedOut(IndexChildTimeout {
                trigger: DeadlineTrigger::NoProgress,
                stop: ChildStop::Cooperative,
            })
        );
    }

    #[tokio::test]
    async fn the_absolute_ceiling_stops_a_child_that_keeps_reporting_progress() {
        let directory = workspace();
        let child = fixture_child(
            directory.path(),
            &format!(
                "( i=0; while :; do printf '{{\"progress\":{{\"completed_items\":%d}}}}\\n' \"$i\" >&2; i=$((i+1)); sleep 0.05; done ) &\nticker=$!\ncat > /dev/null\nkill $ticker\n{CANCELLED_FAILURE}\nexit 1"
            ),
        );
        let outcome = run_index_child(
            child,
            IndexChildPolicy {
                ceiling: Duration::from_millis(900),
                ..TEST_POLICY
            },
        )
        .await;
        assert_eq!(
            outcome,
            IndexChildOutcome::TimedOut(IndexChildTimeout {
                trigger: DeadlineTrigger::Ceiling,
                stop: ChildStop::Cooperative,
            })
        );
    }

    #[tokio::test]
    async fn a_child_that_ignores_the_stop_request_is_killed_only_after_the_grace() {
        let directory = workspace();
        // Never reads stdin and never exits on its own.
        let child = fixture_child(directory.path(), "while :; do sleep 0.05; done");
        let grace = Duration::from_millis(800);
        let started = std::time::Instant::now();
        let outcome = run_index_child(
            child,
            IndexChildPolicy {
                inactivity: Duration::from_millis(200),
                grace,
                ..TEST_POLICY
            },
        )
        .await;
        assert_eq!(
            outcome,
            IndexChildOutcome::TimedOut(IndexChildTimeout {
                trigger: DeadlineTrigger::NoProgress,
                stop: ChildStop::Forced,
            })
        );
        assert!(started.elapsed() >= grace);
    }

    #[tokio::test]
    async fn an_exit_without_confirmed_cleanup_is_not_reported_as_cooperative() {
        let directory = workspace();
        for failure in [
            // The cancellation is the primary failure, but PostgreSQL did not
            // confirm the cleanup of what the child held.
            r#"printf '{\n  "error": {\n    "code": "request_cancelled",\n    "previous_generation_visible": true,\n    "cleanup_failure": {\n      "code": "index_cleanup_failed",\n      "message": "cleanup failed"\n    }\n  }\n}\n' >&2"#,
            r#"printf '{\n  "error": {\n    "code": "index_cleanup_failed"\n  }\n}\n' >&2"#,
        ] {
            let child = fixture_child(
                directory.path(),
                &format!("cat > /dev/null\n{failure}\nexit 1"),
            );
            let outcome = run_index_child(child, TEST_POLICY).await;
            assert_eq!(
                outcome,
                IndexChildOutcome::TimedOut(IndexChildTimeout {
                    trigger: DeadlineTrigger::NoProgress,
                    stop: ChildStop::Unconfirmed,
                }),
                "{failure}"
            );
        }
    }

    #[tokio::test]
    async fn a_failure_keeps_only_the_final_validated_error_code() {
        let directory = workspace();
        let child = fixture_child(
            directory.path(),
            "printf '%s\\n' '{\"progress\":{\"completed_items\":1}}' >&2\nprintf 'cartograph: warning text { not json\\n' >&2\nprintf '{\\n  \"error\": {\\n    \"code\": \"lease_busy\",\\n    \"message\": \"busy\"\\n  }\\n}\\n' >&2\nexit 1",
        );
        assert_eq!(
            run_index_child(child, TEST_POLICY).await,
            IndexChildOutcome::Failed {
                code: Some("lease_busy".to_owned())
            }
        );

        let child = fixture_child(
            directory.path(),
            "printf '{\\n  \"error\": {\\n    \"code\": \"Not A Code\"\\n  }\\n}\\n' >&2\nexit 1",
        );
        assert_eq!(
            run_index_child(child, TEST_POLICY).await,
            IndexChildOutcome::Failed { code: None }
        );
    }

    #[tokio::test]
    async fn an_oversized_report_is_rejected_but_drained_so_the_child_can_exit() {
        const BOUND: usize = 16;
        const WRITTEN: usize = BOUND * 4;
        let mut oversized = std::io::Cursor::new(vec![b'x'; WRITTEN]);
        assert_eq!(read_bounded(&mut oversized, BOUND).await, None);
        // Stopping at the bound would leave the child blocked on its write.
        assert_eq!(usize::try_from(oversized.position()), Ok(WRITTEN));
        let mut fitting = std::io::Cursor::new(b"{}".to_vec());
        assert_eq!(
            read_bounded(&mut fitting, BOUND).await,
            Some(b"{}".to_vec())
        );
    }

    #[tokio::test]
    async fn a_report_of_a_checkout_changed_after_publication_is_decoded_strictly() {
        let directory = workspace();
        let child = fixture_child(
            directory.path(),
            "printf '%s\\n' '{\"generation_id\":\"g-2\",\"source_revision\":\"r-2\",\"live_source\":\"changed_after_publication\"}'",
        );
        assert_eq!(
            run_index_child(child, TEST_POLICY).await,
            IndexChildOutcome::Completed(IndexChildReport {
                generation_id: "g-2".to_owned(),
                source_revision: "r-2".to_owned(),
                changed_after_publication: true,
            })
        );
        // An unknown agreement value is never guessed to be a match.
        let child = fixture_child(
            directory.path(),
            "printf '%s\\n' '{\"generation_id\":\"g-2\",\"source_revision\":\"r-2\",\"live_source\":\"unknown\"}'",
        );
        assert_eq!(
            run_index_child(child, TEST_POLICY).await,
            IndexChildOutcome::Failed { code: None }
        );
    }
}
