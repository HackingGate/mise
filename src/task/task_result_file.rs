//! The run-result file: a local, versioned record of what one `mise run` did.
//!
//! The file holds JSON Lines. A run writes a `run` record when it starts, one
//! `task` record per task, and an `end` record once it has finished, so a
//! reader can tell a complete record from one cut short by a killed process.
//! A nested `mise run` started by a task appends its own records to the same
//! file, under its own invocation id, naming its parent's.
//!
//! The format is documented in `docs/tasks/run-results.md`; change
//! [`SCHEMA_VERSION`] whenever a change would break a reader.

use crate::config::{Config, Settings};
use crate::task::task_executor::{TaskCacheDecision, TaskRunOutcome, TaskRunState};
use crate::task::{Task, TaskKey, TaskRunPhase, task_key};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The version of the record format. A reader rejects a version it does not know.
pub const SCHEMA_VERSION: u32 = 1;

/// Set for every task, so a nested `mise run` appends to the same file.
pub const FILE_ENV: &str = "MISE_TASK_RESULT_FILE";
/// Set for every task to the invocation id of the run that started it.
pub const PARENT_ENV: &str = "__MISE_TASK_RESULT_PARENT";

/// What happened to a task, as the run-result file reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskResultState {
    /// The task's commands ran and succeeded.
    Executed,
    /// The task's outputs were restored from the artifact cache.
    Restored,
    /// Nothing ran: the task was skipped or already up to date.
    Skipped,
    /// The task failed.
    Failed,
    /// The task was stopped because another task failed, the run was interrupted, or it
    /// was still running when the run ended.
    Cancelled,
    /// The task never started, because the run stopped before it was reached.
    NotRun,
}

impl From<TaskRunState> for TaskResultState {
    fn from(state: TaskRunState) -> Self {
        match state {
            TaskRunState::Executed => Self::Executed,
            TaskRunState::Restored => Self::Restored,
            TaskRunState::Skipped => Self::Skipped,
        }
    }
}

/// Which task a record is about. Arguments are redacted like terminal output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskIdentity {
    pub name: String,
    pub args: Vec<String>,
    pub phase: TaskRunPhase,
    pub source: PathBuf,
    pub config_root: Option<PathBuf>,
    /// The task's direct dependencies in this run.
    pub depends: Vec<DependencyIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DependencyIdentity {
    pub name: String,
    pub args: Vec<String>,
}

impl TaskIdentity {
    pub fn new(task: &Task, depends: &[TaskKey], config: Option<&Config>) -> Self {
        let redact = |args: &[String]| -> Vec<String> {
            args.iter()
                .map(|a| config.map_or_else(|| a.clone(), |c| c.redact(a)))
                .collect()
        };
        let mut depends: Vec<_> = depends
            .iter()
            .map(|(name, args, _, _)| DependencyIdentity {
                name: name.clone(),
                args: redact(args),
            })
            .collect();
        depends.sort();
        depends.dedup();
        Self {
            name: task.name.clone(),
            args: redact(&task.args),
            phase: task.run_phase,
            source: task.config_source.clone(),
            config_root: task.config_root.clone(),
            depends,
        }
    }
}

/// One task's result.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub state: TaskResultState,
    pub cache: Option<TaskCacheDecision>,
    pub exit_code: Option<i32>,
    pub start: Option<SystemTime>,
    pub end: Option<SystemTime>,
}

impl TaskResult {
    /// The result of a task that finished, with or without an error.
    pub fn finished(
        outcome: &Result<TaskRunOutcome>,
        cache: Option<TaskCacheDecision>,
        cancelled: bool,
        exit_code: Option<i32>,
        start: SystemTime,
        end: SystemTime,
    ) -> Self {
        let (state, exit_code) = match outcome {
            Ok(outcome) if outcome.state == TaskRunState::Skipped => {
                (TaskResultState::Skipped, None)
            }
            Ok(outcome) => (outcome.state.into(), Some(0)),
            Err(_) if cancelled => (TaskResultState::Cancelled, exit_code),
            Err(_) => (TaskResultState::Failed, exit_code),
        };
        let cache = match outcome {
            Ok(outcome) => outcome.cache.clone(),
            Err(_) => cache,
        };
        Self {
            state,
            cache,
            exit_code,
            start: Some(start),
            end: Some(end),
        }
    }
}

#[derive(Serialize)]
struct CacheRecord<'a> {
    enabled: bool,
    key: Option<&'a str>,
    result: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record<'a> {
    Run {
        schema_version: u32,
        invocation_id: &'a str,
        parent_invocation_id: Option<&'a str>,
        mise_version: &'static str,
        cwd: Option<&'a Path>,
        start_unix_ns: u64,
    },
    Task {
        invocation_id: &'a str,
        #[serde(flatten)]
        task: &'a TaskIdentity,
        state: TaskResultState,
        cache: Option<CacheRecord<'a>>,
        exit_code: Option<i32>,
        start_unix_ns: Option<u64>,
        end_unix_ns: Option<u64>,
        duration_ns: Option<u64>,
    },
    End {
        invocation_id: &'a str,
        completed: bool,
        exit_code: i32,
        end_unix_ns: u64,
        duration_ns: u64,
    },
}

fn unix_ns(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn duration_ns(start: SystemTime, end: SystemTime) -> u64 {
    end.duration_since(start)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The run-result file of one `mise run` invocation.
pub struct TaskResultFile {
    path: PathBuf,
    invocation_id: String,
    start: SystemTime,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// `None` once a write failed: the run then never writes its `end` record.
    file: Option<File>,
    /// The tasks the run planned, in order, to report those that never ran.
    planned: Vec<(TaskKey, TaskIdentity)>,
    started: HashSet<TaskKey>,
    recorded: HashSet<TaskKey>,
    /// Set when mise stops the tasks still running, as `--timeout` does.
    stopping: bool,
    finished: bool,
}

impl TaskResultFile {
    /// Open the file named by `task.result_file`, if it is set. A run started by a task of
    /// another `mise run` appends to the file; any other run replaces it.
    pub fn open_if_enabled() -> Result<Option<Self>> {
        let Some(path) = Settings::get().task.result_file.clone() else {
            return Ok(None);
        };
        Settings::get().ensure_experimental("task.result_file")?;
        let path = match &*crate::dirs::CWD {
            Some(cwd) if path.is_relative() => cwd.join(path),
            _ => path,
        };
        let parent = std::env::var(PARENT_ENV).ok().filter(|p| !p.is_empty());
        Self::create(path, parent).map(Some)
    }

    /// Open `path` and write the `run` record. Without a parent the file is truncated.
    pub fn create(path: PathBuf, parent_invocation_id: Option<String>) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .wrap_err_with(|| format!("cannot open task result file {}", path.display()))?;
        if parent_invocation_id.is_none() {
            file.set_len(0)
                .wrap_err_with(|| format!("cannot truncate task result file {}", path.display()))?;
        }
        let this = Self {
            path,
            invocation_id: uuid::Uuid::now_v7().to_string(),
            start: SystemTime::now(),
            state: Mutex::new(State {
                file: Some(file),
                ..Default::default()
            }),
        };
        let cwd = crate::dirs::CWD.clone();
        let mut state = this.state.lock().unwrap();
        this.write(
            &mut state,
            &Record::Run {
                schema_version: SCHEMA_VERSION,
                invocation_id: &this.invocation_id,
                parent_invocation_id: parent_invocation_id.as_deref(),
                mise_version: env!("CARGO_PKG_VERSION"),
                cwd: cwd.as_deref(),
                start_unix_ns: unix_ns(this.start),
            },
        );
        if state.file.is_none() {
            eyre::bail!("cannot write task result file {}", this.path.display());
        }
        drop(state);
        Ok(this)
    }

    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// The variables a task gets so that a nested `mise run` appends to this file.
    pub fn task_env(&self) -> [(&'static str, String); 2] {
        [
            (FILE_ENV, self.path.to_string_lossy().into_owned()),
            (PARENT_ENV, self.invocation_id.clone()),
        ]
    }

    /// Remember the tasks the run is about to schedule, so that the `end` record can be
    /// preceded by a record for each one that never ran.
    pub fn plan(&self, tasks: impl IntoIterator<Item = (TaskKey, TaskIdentity)>) {
        self.state.lock().unwrap().planned.extend(tasks);
    }

    /// Note that a task has started, so a run that ends while it is running reports it
    /// as cancelled rather than not run.
    pub fn start_task(&self, task: &Task) {
        self.state.lock().unwrap().started.insert(task_key(task));
    }

    /// Note that mise is about to stop every running task, so a task that fails from
    /// then on is reported as cancelled.
    pub fn stop_running_tasks(&self) {
        self.state.lock().unwrap().stopping = true;
    }

    /// Write a task's record. Ignored after the `end` record.
    pub fn record_task(&self, task: &Task, identity: &TaskIdentity, result: &TaskResult) {
        let mut state = self.state.lock().unwrap();
        if state.finished {
            return;
        }
        state.recorded.insert(task_key(task));
        if state.stopping && result.state == TaskResultState::Failed {
            let result = TaskResult {
                state: TaskResultState::Cancelled,
                ..result.clone()
            };
            self.write_task(&mut state, identity, &result);
        } else {
            self.write_task(&mut state, identity, result);
        }
    }

    /// Write a record for each planned task that has none, then the `end` record.
    /// Idempotent. A task still running is reported as cancelled; one that never
    /// started, as not run.
    pub fn finish(&self, exit_code: i32) {
        let mut state = self.state.lock().unwrap();
        if state.finished {
            return;
        }
        state.finished = true;
        let planned = std::mem::take(&mut state.planned);
        for (key, identity) in &planned {
            if !state.recorded.insert(key.clone()) {
                continue;
            }
            let state_value = if state.started.contains(key) {
                TaskResultState::Cancelled
            } else {
                TaskResultState::NotRun
            };
            let result = TaskResult {
                state: state_value,
                cache: None,
                exit_code: None,
                start: None,
                end: None,
            };
            self.write_task(&mut state, identity, &result);
        }
        let end = SystemTime::now();
        self.write(
            &mut state,
            &Record::End {
                invocation_id: &self.invocation_id,
                completed: true,
                exit_code,
                end_unix_ns: unix_ns(end),
                duration_ns: duration_ns(self.start, end),
            },
        );
        state.file = None;
    }

    fn write_task(&self, state: &mut State, identity: &TaskIdentity, result: &TaskResult) {
        let cache = result.cache.as_ref().map(|cache| CacheRecord {
            enabled: true,
            key: Some(&cache.key),
            result: cache.result.as_str(),
        });
        let cache = match (result.state, cache) {
            (TaskResultState::NotRun | TaskResultState::Cancelled, None) => None,
            (_, None) => Some(CacheRecord {
                enabled: false,
                key: None,
                result: "disabled",
            }),
            (_, cache) => cache,
        };
        self.write(
            state,
            &Record::Task {
                invocation_id: &self.invocation_id,
                task: identity,
                state: result.state,
                cache,
                exit_code: result.exit_code,
                start_unix_ns: result.start.map(unix_ns),
                end_unix_ns: result.end.map(unix_ns),
                duration_ns: result.start.zip(result.end).map(|(s, e)| duration_ns(s, e)),
            },
        );
    }

    /// Append one record as a single write, so records of a nested run that shares the
    /// file are never interleaved within a line. After a failed write the file is
    /// dropped, so the run never claims to be complete.
    fn write(&self, state: &mut State, record: &Record<'_>) {
        let Some(file) = state.file.as_mut() else {
            return;
        };
        let line = match serde_json::to_string(record) {
            Ok(json) => json + "\n",
            Err(err) => {
                warn!("cannot encode task result record: {err}");
                state.file = None;
                return;
            }
        };
        if let Err(err) = file.write_all(line.as_bytes()) {
            warn!(
                "cannot write task result file {}: {err}",
                self.path.display()
            );
            state.file = None;
        }
    }
}

/// Every planned task's identity, in the order the graph created them.
pub fn planned_identities(
    deps: &crate::task::Deps,
    config: &Config,
) -> Vec<(TaskKey, TaskIdentity)> {
    deps.all_in_creation_order()
        .into_iter()
        .map(|task| {
            (
                task_key(task),
                TaskIdentity::new(task, &deps.direct_dependencies(task), Some(config)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::task_executor::TaskCacheResult;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn read_records(path: &Path) -> Vec<BTreeMap<String, serde_json::Value>> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn task(name: &str) -> Task {
        Task {
            name: name.to_string(),
            config_source: PathBuf::from("/project/mise.toml"),
            config_root: Some(PathBuf::from("/project")),
            ..Default::default()
        }
    }

    fn identity(task: &Task, depends: &[&Task]) -> TaskIdentity {
        let keys: Vec<_> = depends.iter().map(|t| task_key(t)).collect();
        TaskIdentity::new(task, &keys, None)
    }

    fn ok(state: TaskRunState, cache: Option<TaskCacheDecision>) -> Result<TaskRunOutcome> {
        Ok(TaskRunOutcome {
            state,
            cache_key: cache.as_ref().map(|c| c.key.clone()),
            cache,
        })
    }

    fn decision(result: TaskCacheResult) -> Option<TaskCacheDecision> {
        Some(TaskCacheDecision {
            key: "k".to_string(),
            result,
        })
    }

    fn finished(
        outcome: Result<TaskRunOutcome>,
        cache: Option<TaskCacheDecision>,
        cancelled: bool,
        exit_code: Option<i32>,
    ) -> TaskResult {
        let start = UNIX_EPOCH + std::time::Duration::from_secs(10);
        let end = start + std::time::Duration::from_millis(5);
        TaskResult::finished(&outcome, cache, cancelled, exit_code, start, end)
    }

    #[test]
    fn a_complete_run_has_a_header_task_records_and_an_end_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let file = TaskResultFile::create(path.clone(), None).unwrap();
        let build = task("build");
        let test = task("test");
        file.plan([
            (task_key(&build), identity(&build, &[])),
            (task_key(&test), identity(&test, &[&build])),
        ]);
        file.start_task(&build);
        file.record_task(
            &build,
            &identity(&build, &[]),
            &finished(
                ok(TaskRunState::Restored, decision(TaskCacheResult::Hit)),
                None,
                false,
                None,
            ),
        );
        file.start_task(&test);
        file.record_task(
            &test,
            &identity(&test, &[&build]),
            &finished(ok(TaskRunState::Executed, None), None, false, None),
        );
        file.finish(0);

        let records = read_records(&path);
        assert_eq!(records.len(), 4);
        assert_eq!(records[0]["type"], json!("run"));
        assert_eq!(records[0]["schema_version"], json!(SCHEMA_VERSION));
        assert_eq!(records[0]["parent_invocation_id"], json!(null));
        let id = json!(file.invocation_id());
        assert!(records.iter().all(|r| r["invocation_id"] == id));

        assert_eq!(records[1]["type"], json!("task"));
        assert_eq!(records[1]["name"], json!("build"));
        assert_eq!(records[1]["state"], json!("restored"));
        assert_eq!(records[1]["config_root"], json!("/project"));
        assert_eq!(records[1]["source"], json!("/project/mise.toml"));
        assert_eq!(
            records[1]["cache"],
            json!({"enabled": true, "key": "k", "result": "hit"})
        );
        assert_eq!(records[1]["exit_code"], json!(0));
        assert_eq!(records[1]["start_unix_ns"], json!(10_000_000_000u64));
        assert_eq!(records[1]["end_unix_ns"], json!(10_005_000_000u64));
        assert_eq!(records[1]["duration_ns"], json!(5_000_000u64));

        assert_eq!(records[2]["state"], json!("executed"));
        assert_eq!(
            records[2]["depends"],
            json!([{"name": "build", "args": []}])
        );
        assert_eq!(
            records[2]["cache"],
            json!({"enabled": false, "key": null, "result": "disabled"})
        );

        assert_eq!(records[3]["type"], json!("end"));
        assert_eq!(records[3]["completed"], json!(true));
        assert_eq!(records[3]["exit_code"], json!(0));
    }

    #[test]
    fn results_map_outcomes_to_states() {
        let skipped = finished(
            ok(TaskRunState::Skipped, decision(TaskCacheResult::Fresh)),
            None,
            false,
            None,
        );
        assert_eq!(skipped.state, TaskResultState::Skipped);
        assert_eq!(skipped.exit_code, None);
        assert_eq!(skipped.cache, decision(TaskCacheResult::Fresh));

        let failed = finished(
            Err(eyre::eyre!("boom")),
            decision(TaskCacheResult::Miss),
            false,
            Some(3),
        );
        assert_eq!(failed.state, TaskResultState::Failed);
        assert_eq!(failed.exit_code, Some(3));
        assert_eq!(failed.cache, decision(TaskCacheResult::Miss));

        let cancelled = finished(Err(eyre::eyre!("killed")), None, true, None);
        assert_eq!(cancelled.state, TaskResultState::Cancelled);
        assert_eq!(cancelled.exit_code, None);

        let forced = finished(
            ok(TaskRunState::Executed, decision(TaskCacheResult::Forced)),
            None,
            true,
            None,
        );
        assert_eq!(forced.state, TaskResultState::Executed);
        assert_eq!(forced.exit_code, Some(0));
    }

    #[test]
    fn finish_reports_planned_tasks_that_never_ran_or_never_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let file = TaskResultFile::create(path.clone(), None).unwrap();
        let fail = task("fail");
        let slow = task("slow");
        let after = task("after");
        file.plan([
            (task_key(&fail), identity(&fail, &[])),
            (task_key(&slow), identity(&slow, &[])),
            (task_key(&after), identity(&after, &[&fail])),
        ]);
        file.start_task(&fail);
        file.start_task(&slow);
        file.record_task(
            &fail,
            &identity(&fail, &[]),
            &finished(Err(eyre::eyre!("boom")), None, false, Some(3)),
        );
        file.finish(3);
        // A task that reports after the end record is ignored.
        file.record_task(
            &slow,
            &identity(&slow, &[]),
            &finished(Err(eyre::eyre!("late")), None, true, None),
        );
        file.finish(3);

        let records = read_records(&path);
        let states: Vec<_> = records
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r["type"].as_str().unwrap(),
                    r.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| format!("{n}={}", r["state"].as_str().unwrap()))
                        .unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                "run ",
                "task fail=failed",
                "task slow=cancelled",
                "task after=not_run",
                "end ",
            ]
        );
        assert_eq!(records[3]["cache"], json!(null));
        assert_eq!(records[3]["start_unix_ns"], json!(null));
        assert_eq!(records[4]["exit_code"], json!(3));
    }

    #[test]
    fn a_task_that_fails_after_mise_stops_the_run_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let file = TaskResultFile::create(path.clone(), None).unwrap();
        let slow = task("slow");
        file.start_task(&slow);
        file.stop_running_tasks();
        file.record_task(
            &slow,
            &identity(&slow, &[]),
            &finished(Err(eyre::eyre!("terminated")), None, false, None),
        );
        file.finish(1);
        let records = read_records(&path);
        assert_eq!(records[1]["state"], json!("cancelled"));
        assert_eq!(records[2]["exit_code"], json!(1));
    }

    #[test]
    fn a_nested_run_appends_under_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let parent = TaskResultFile::create(path.clone(), None).unwrap();
        let child =
            TaskResultFile::create(path.clone(), Some(parent.invocation_id().to_string())).unwrap();
        child.finish(0);
        parent.finish(0);

        let records = read_records(&path);
        assert_eq!(records.len(), 4);
        assert_eq!(records[1]["type"], json!("run"));
        assert_eq!(
            records[1]["parent_invocation_id"],
            json!(parent.invocation_id())
        );
        assert_ne!(child.invocation_id(), parent.invocation_id());

        // A new top-level run replaces what an earlier one wrote.
        let next = TaskResultFile::create(path.clone(), None).unwrap();
        let records = read_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["invocation_id"], json!(next.invocation_id()));
    }

    #[test]
    fn task_env_points_a_nested_run_at_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let file = TaskResultFile::create(path.clone(), None).unwrap();
        let env: BTreeMap<_, _> = file.task_env().into_iter().collect();
        assert_eq!(env[FILE_ENV], path.to_string_lossy());
        assert_eq!(env[PARENT_ENV], file.invocation_id());
    }
}
