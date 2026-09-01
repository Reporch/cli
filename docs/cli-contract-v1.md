# Reporch CLI 1.x compatibility contract

This document freezes the automation-facing surface of Reporch CLI 1.x.
Additive commands, optional fields, and new enum values may be introduced in a
minor release. Existing command names, option meanings, envelope fields, and
exit-code meanings are removed or changed only in 2.0.

## Global behavior

The global options are `--cwd`, `--profile`, `--format`, `--json`,
`--no-input`, `--yes`, `--quiet`, `--verbose`, and `--color`. Configuration is
resolved in this order:

1. command-line flags;
2. `REPORCH_*` environment variables;
3. project configuration;
4. user configuration;
5. built-in defaults.

`--no-input` prohibits prompts. A command that needs confirmation must instead
receive `--yes` or fail with exit code 2. Progress and explanatory text never
appear on stdout when `--format json` or `--format jsonl` is selected. One-shot
JSON keeps stderr to one final error envelope. JSONL may emit
`reporch.cli-progress.v1` envelopes on stderr before the final result or error.

`--profile NAME` (or `REPORCH_PROFILE`) selects a named user profile from
`config.toml`. A direct CLI option overrides its matching `REPORCH_*`
environment variable, which overrides the user profile. Project authoring
settings then apply to project fields, followed by built-in defaults. Project
files cannot select API or OAuth destinations: accepting remote endpoints from
an untrusted checkout could disclose credentials. A user profile contains only
connection metadata and never tokens.

## Stable command surface

The 1.x command families are:

- `new`, `migrate`, `check`, `status`, `diff`, `doctor`, `completion`, `verify`, `submit`, and the
  `publish` shorthand;
- `auth login|status|logout`;
- `project init|create|link|list|show|status|diff|pull|push|open|validate|package`;
- `statement`, `test`, `generator`, `validator`, `checker`, `solution`,
  `interactor`, `grader`, and `output` authoring commands;
- `member search|list|add|update|remove`;
- `review submit|request|inbox|show|list|status|claim|cancel|approve|request-changes`;
- `waiver list|create|revoke`;
- `validation list|show|watch` and `events watch`;
- `release build|list|show|download`;
- `publication publish|status` and `quota show`;
- `revision list|show|diff|restore`;
- `manifest validate|digest|compatibility` and `package import|export`;
- `runtime status|doctor|update|repair|reset`;
- `toolchain list|inspect|install|prefetch` and `sandbox plan|run`;
- `artifact verify-minisign` and `desktop verify-updater-artifact`.

The regression suite asserts that these commands remain reachable. Deprecated
pre-1.0 aliases remain supported throughout 1.x and may print a warning only in
human output.

`manifest validate`, `manifest digest`, and `manifest compatibility` default to
the current project's `reporch.yaml`; an explicit path remains supported.
`manifest compatibility` and `package export` default to the manifest's
`package_profile`; export defaults its source root to the manifest directory.
`--require-exportable` (with `--strict` retained as an alias) exits 1 for a
blocked projection, while inspection without it remains a successful query.
Hyphenated package-profile names are canonical CLI spelling and underscore
spellings are accepted aliases. `test case add --input` and `--answer` always
mean files; `--input-text` and `--answer-text` create collision-resistant files
inside the project and roll them back if the manifest edit fails. Validator
unit inputs use the same `--input INPUT_FILE` or `--input-text TEXT` contract.
`test group add` names its positional value `NAME`; generated UUIDs remain an
internal V2 identity. Supplying `--minimum-score` and `--maximum-score` to
`solution update` updates an existing partial verdict range even when
`--expected partial` is omitted.
Interactive and grader runtime commands accept a solution name, UUID, or source
path and a test name, UUID, or input path. These readable selectors are aliases
for the same stable project entities and do not change JSON result fields.
`status` and `diff` are aliases for `project status` and `project diff`.
`checker test` is an alias for `checker run`. `statement add --create` safely
creates a missing Markdown starter without changing the legacy no-overwrite
behavior when the flag is absent. `reporch new` includes a portable validator
starter; `project init --portable` opts into the same files.

## JSON success and error envelopes

One-shot JSON success is written to stdout:

```json
{
  "schema": "reporch.cli-result.v1",
  "command": "project status",
  "data": {}
}
```

JSON errors are written to stderr and stdout remains empty:

```json
{
  "schema": "reporch.cli-error.v1",
  "command": "project push",
  "error_code": "working_copy.revision_conflict",
  "message": "...",
  "retryable": false,
  "trace_id": "019...",
  "details": {}
}
```

Consumers must ignore unknown object fields. Existing fields retain their type
and meaning in 1.x. `details` is optional and contains a command-specific,
schema-tagged object when machine-readable recovery or evidence is available;
`message` remains human-readable. Unbounded event streams require `--format jsonl`; every line
is one complete `reporch.cli-result.v1` envelope. `--format json` is accepted
only when a command has a finite response or an explicit finite event bound.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Domain operation completed with a failed verdict or release state |
| 2 | Invalid command, configuration, or local input |
| 3 | Revision or ETag conflict |
| 4 | Authentication required |
| 5 | Permission, policy, trust, or quota denial |
| 6 | Retryable infrastructure failure |
| 7 | User cancellation |
| 130 | SIGINT |

Server `error_code` and `trace_id` values are preserved when available.

## File and credential guarantees

`reporch.yaml` is the human-editable source. `.reporch/state.json` contains no
tokens, is replaceable from server state, and is written with private
permissions. OAuth refresh credentials are stored only in the operating system
credential store. Downloads and revision restores never overwrite existing
files. `project init --allow-non-empty` is an explicit opt-in that preflights
every generated path, rejects stale local project state, and refuses collisions
or symlinked parents before a capability-scoped, rollback-safe write transaction.
The transaction journal and every staged file are durable before generated files
are published. A later `project init` removes only reserved staging paths if
commit never began; once any final path was published, it completes the commit
only after bounded digest and V1/V2 semantic validation. It never deletes a
user-visible final path during crash recovery, and changed files fail closed.
Removing an output submission may prune declarations that are no longer
referenced, but never deletes the corresponding local files. Local author-code
execution defaults to the mandatory signed Reporch VM Runtime, never falls back
to direct host execution, and cannot create official release evidence. Explicit
`podman` and `docker` runtime selectors are deprecated 1.x compatibility paths.
