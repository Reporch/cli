# Zero-cost GitHub Actions

This public repository uses only compute that GitHub documents as free:

- standard GitHub-hosted runners for native macOS, Linux, and Windows build and
  installation qualification;
- owned self-hosted runners carrying the `cli-zero-cost` label for runtime
  image construction, signing, fuzzing, and stability monitoring.

GitHub-hosted larger runners and custom runner sizes are never allowed. The
workflow contract keeps an exact allowlist of standard runner labels so a
future edit cannot silently select billable compute. GitHub documents standard
hosted runner use as free and unlimited for public repositories in
[Choosing the runner for a job](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job#standard-github-hosted-runners-for-public-repositories).

`SELF_HOSTED_ACTIONS_ALLOWED_ACTOR` must contain the single trusted GitHub actor
allowed to dispatch work. Pull requests from other actors are intentionally not
executed on persistent runners; maintainers reproduce them in a clean local
worktree before merging.

The initial Apple silicon runner can be registered with:

```sh
bash deploy/self-hosted/install-macos-runner.sh
```

The five official native targets run on `macos-15`, `macos-15-intel`,
`ubuntu-24.04-arm`, `ubuntu-24.04`, and `windows-2025`. Runtime and toolchain
image workflows remain on the owned Apple silicon runner because they require
local VM hardware, isolated signing material, and long deterministic rebuilds.
