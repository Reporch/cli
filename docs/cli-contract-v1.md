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
appear on stdout when `--format json` or `--format jsonl` is selected.

`--profile NAME` (or `REPORCH_PROFILE`) selects a named user profile from
`config.toml`. A direct CLI option overrides its matching `REPORCH_*`
environment variable, which overrides the user profile. Project authoring
settings then apply to project fields, followed by built-in defaults. Project
files cannot select API or OAuth destinations: accepting remote endpoints from
an untrusted checkout could disclose credentials. A user profile contains only
connection metadata and never tokens.

## Stable command surface

The 1.x command families are:

- `migrate`, `check`, `doctor`, `completion`, `verify`, and `submit`;
- `auth login|status|logout`;
- `project init|create|link|list|show|status|diff|pull|push|open|validate|package`;
- `statement`, `test`, `generator`, `validator`, `checker`, `solution`,
  `interactor`, `grader`, and `output` authoring commands;
- `member search|list|add|update|remove`;
- `review submit|request|inbox|status|claim|cancel|approve|request-changes`;
- `waiver list|create|revoke`;
- `validation list|show|watch` and `events watch`;
- `release build|list|show|download`;
- `publication publish|status` and `quota show`;
- `revision list|show|diff|restore`;
- `manifest validate|digest|compatibility` and `package import|export`;
- `toolchain list|inspect|install` and `sandbox plan|run`;
- `artifact verify-minisign` and `desktop verify-updater-artifact`.

The regression suite asserts that these commands remain reachable. Deprecated
pre-1.0 aliases remain supported throughout 1.x and may print a warning only in
human output.

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
  "trace_id": "019..."
}
```

Consumers must ignore unknown object fields. Existing fields retain their type
and meaning in 1.x. Unbounded event streams require `--format jsonl`; every line
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
files. Local execution is opt-in and cannot create official release evidence.
