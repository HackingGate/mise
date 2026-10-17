---
description: "Record what each task of a mise run did, restored from cache or executed, in a local versioned JSON Lines file."
socialDescription: "A local, versioned record of what each task of a mise run did."
---

# Run results <Badge type="warning" text="experimental" />

The [`task.result_file`](/configuration/settings.html#task.result_file) setting
makes `mise run` write a record of what it did to a local file: which tasks
executed, which were restored from the [artifact cache](/tasks/caching.html),
which were skipped, failed, cancelled or never started, and the cache key of
each. A script or CI job can read it instead of parsing mise's output, and can
tell a run that finished from one that was killed.

```sh
export MISE_EXPERIMENTAL=1
MISE_TASK_RESULT_FILE=results.jsonl mise run ci
jq -c 'select(.type == "task") | {name, state}' results.jsonl
```

::: warning Experimental
The setting needs `experimental = true`. The records carry a schema version,
which changes whenever a change could break a reader.
:::

## File format

The file holds [JSON Lines](https://jsonlines.org/): one JSON object per line,
each with a `type` and the `invocation_id` of the `mise run` that wrote it.

A top-level `mise run` replaces the file when it starts, before it resolves any
task, and then appends:

1. one `run` record;
2. one `task` record each time a task finishes;
3. a `task` record for every task it planned but never finished, then one `end`
   record, once the run is over.

mise passes the file to every task, so a `mise run` that a task starts appends
its own `run`, `task` and `end` records to the same file, with its own
`invocation_id`. Each record is written with a single append under an
exclusive lock on the file, so records of nested runs never mix within a line.
On a filesystem without file locks, mise falls back to the single append alone.

### `run`

| Field                  | Value                                                                       |
| ---------------------- | --------------------------------------------------------------------------- |
| `schema_version`       | `1`. A reader should reject a version it does not know.                     |
| `invocation_id`        | A new random id for this `mise run`, as 32 hexadecimal digits.              |
| `parent_invocation_id` | The `invocation_id` of the run whose task started this one, otherwise null. |
| `mise_version`         | The mise version, such as `2026.10.12`.                                     |
| `cwd`                  | The directory mise ran in.                                                  |
| `start_unix_ns`        | When the run started, in nanoseconds since the Unix epoch.                  |

### `task`

| Field           | Value                                                                                                                         |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `name`          | The task's full name.                                                                                                         |
| `args`          | Its arguments, redacted as in mise's output.                                                                                  |
| `phase`         | `normal`, or `post` for a task that runs from [`depends_post`](/tasks/task-configuration.html#depends-post).                  |
| `source`        | The config file that defines the task.                                                                                        |
| `config_root`   | The config root of that file.                                                                                                 |
| `depends`       | The task's direct `depends`, each as `{ "name", "args" }`.                                                                    |
| `state`         | What happened to the task. See [States](#states).                                                                             |
| `cache`         | `{ "enabled", "key", "result" }`, as the OpenTelemetry [cache attributes](/tasks/opentelemetry.html#artifact-cache) describe. |
| `exit_code`     | `0` for a task that executed or was restored, the failing command's exit code, or null when there was none.                   |
| `start_unix_ns` | When mise picked the task up. Null for a task that never started.                                                             |
| `end_unix_ns`   | When the task finished: `start_unix_ns + duration_ns`. Null for a task that never started.                                    |
| `duration_ns`   | How long the task took, by a monotonic clock, so a system clock change does not distort it.                                   |

`cache` is null when mise cannot know it: for a task that never started, and
for one still running when the run ended.

### `end`

| Field         | Value                                              |
| ------------- | -------------------------------------------------- |
| `completed`   | Always `true`.                                     |
| `exit_code`   | The exit status of `mise run`.                     |
| `end_unix_ns` | When the run ended: `start_unix_ns + duration_ns`. |
| `duration_ns` | How long the run took, by a monotonic clock.       |

Timestamps are integers above 2<sup>53</sup>, so read them with a JSON parser
that keeps 64-bit integers exact.

## States

| `state`     | Meaning                                                                                                                                             |
| ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `executed`  | The task's commands ran and succeeded.                                                                                                              |
| `restored`  | mise restored the task's outputs from the artifact cache instead of running it.                                                                     |
| `skipped`   | Nothing ran: the task's sources were up to date, its outputs were already current under its cache key, its `skip_if` held, or `task.skip` named it. |
| `failed`    | The task failed.                                                                                                                                    |
| `cancelled` | mise stopped the task: another task failed, the run was interrupted (Ctrl-C), or `--timeout` expired while it ran.                                  |
| `not_run`   | The task never started, because a task it depends on failed or the run was stopped first.                                                           |

## Reading the file safely

A run that mise finishes, successfully or not, ends with an `end` record. A run
that is killed, or that cannot write the file, has none, so treat a file whose
top-level invocation has no `end` record as incomplete. A run that fails to
write a record stops writing and prints a warning; it never writes an `end`
record after a gap.

mise replaces the file when a top-level run starts, so an earlier run's records
do not survive. A failure before that point, such as an invalid config, leaves
the previous file in place: delete it before the run when that matters, or
check that the `run` record's `start_unix_ns` is from this run.

`mise run --dry-run` writes no file.
