# Zero-cost GitHub Actions

Every workflow in this repository is restricted to owned self-hosted runners.
The `cli-zero-cost` label is mandatory and there is no GitHub-hosted fallback.
If an exact target runner is unavailable, its job remains queued instead of
creating billable compute.

`SELF_HOSTED_ACTIONS_ALLOWED_ACTOR` must contain the single trusted GitHub actor
allowed to dispatch work. Pull requests from other actors are intentionally not
executed on persistent runners; maintainers reproduce them in a clean local
worktree before merging.

The initial Apple silicon runner can be registered with:

```sh
bash deploy/self-hosted/install-macos-runner.sh
```

The other official target labels are `cli-macos-x64`, `cli-linux-x64`,
`cli-linux-arm64`, and `cli-windows-x64`. Each label must identify a physically
owned or otherwise zero-incremental-cost machine with the matching native OS.
Do not assign a target label to an emulator or cross compiler: VM and installer
qualification must exercise the real platform backend.
