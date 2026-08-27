# Reporch CLI

The official open-source command-line client for
[Reporch Studio](https://studio.reporch.com). Create algorithm problems,
validate manifests, sync private projects, and import or export Reporch, ICPC,
Polygon-compatible, and DOMjudge packages.

## Install

```bash
npm install --global @reporch/cli
reporch --version
```

The legacy command alias `reporch-studio` invokes the same binary. The npm
package has no lifecycle scripts and does not download executable code during
installation. It selects a platform-specific binary delivered and integrity
checked by npm.

Supported systems:

- macOS arm64 and x64
- Windows x64
- Linux glibc arm64 and x64

The same five binaries are also published as standalone `.tar.gz` or `.zip`
archives on the [GitHub Releases](https://github.com/Reporch/cli/releases)
page. Download the archive and `SHA256SUMS` from the same release, verify the
checksum before extraction, then place `reporch` (or `reporch.exe`) on your
`PATH`. Each archive also carries `LICENSE`, `NOTICE`, and this README.

## Start a problem

```bash
reporch auth login
reporch project create --title "My problem" --directory ./my-problem
cd my-problem
# Edit the generated statements/ko.md and solutions/accepted.* starter files.
reporch test                         # interactive, line-oriented guide
reporch check                        # static and completely offline
reporch submit                       # push, Studio verification, review request
```

`reporch.yaml` is the human-editable source. `reporch.problem.json` is generated
only after the server has assigned the immutable commit ID and bound every file
hash. UUIDs, SHA-256 values, and manifest internals never need to be entered by
the author.

`reporch check` validates the schema, paths, references, files, scoring groups,
and solution roles. It never executes solutions, generators, validators, or
checkers, and reports those unexecuted counts in both human and JSON output.
Run `reporch verify` after linking and pushing for official Studio execution
evidence. Component-specific local commands remain optional preflight checks.

Every non-output-only problem has exactly one accepted `reference` solution.
The starter already provides it. To replace it explicitly:

```bash
reporch solution update accepted --role alternative
reporch solution add --name my-reference --source solutions/my-reference.cpp \
  --language cpp --expected accepted --role reference
```

The same flow is deterministic in CI:

```bash
reporch --format json --no-input check
reporch --format json --no-input project push --message "$GIT_SHA"
reporch --format json --no-input verify
reporch --format json --no-input review submit
```

Every JSON result uses `reporch.cli-result.v1`; every JSON error uses
`reporch.cli-error.v1`. Stable exit codes distinguish domain failure (`1`),
invalid input (`2`), revision conflict (`3`), authentication (`4`), policy or
quota denial (`5`), retryable infrastructure failure (`6`), and cancellation
(`7`).

The complete 1.x automation compatibility promise is documented in
[`docs/cli-contract-v1.md`](docs/cli-contract-v1.md) and enforced by executable
command-surface regression tests.

The server rejects self-approval: the commit author and final reviewer must be
different Reporch subjects, and every approval is bound to the exact commit,
validation run, manifest digest, and reviewer entitlement version.

If a project has no independent reviewer, request the Reporch review pool:

```bash
reporch review request --review-id <REVIEW_ID> --pool
reporch review status --pool-request-id <POOL_REQUEST_ID>

# Accounts with the dedicated reviewer entitlement:
reporch review inbox
reporch review claim --pool-request-id <POOL_REQUEST_ID>
reporch review approve --pool-request-id <POOL_REQUEST_ID> \
  --comment "Checked statement, tests, and expected verdicts"
```

A pool claim is a candidate-bound read/comment/review capability, not project
membership. It disappears after a decision or cancellation. A new commit
invalidates the request, assignment, and approval; concurrent claims are
accepted only once. Removing the reviewer's entitlement also makes the approval
unusable for release.

Create a completely local project without signing in:

```bash
reporch project init \
  --title "My problem" \
  --problem-type standard \
  --directory ./my-problem
```

The available problem types are `standard`, `scored`, `interactive`,
`output-only`, `library`, and `grader`. Authoring commands cover statements,
manual tests and groups, deterministic generators, validator/checker unit
cases, expected solution verdicts and score ranges, interactors, graders, and
output-only mappings. Every edit validates and atomically replaces
`reporch.yaml`.

## Migrate an existing checkout

```bash
reporch migrate                 # previews and confirms in a TTY
reporch --no-input --yes migrate # required form in CI
```

Migration creates `reporch.problem.pre-1.0.json` once, writes `reporch.yaml`
atomically, and checks that the generated immutable manifest has the same
meaning and file hashes. The backup is never overwritten.

## Package compatibility

```bash
reporch manifest compatibility reporch.yaml \
  --profile icpc202509 --strict

reporch package export reporch.yaml problem.zip \
  --profile domjudge-zip

reporch package import polygon-package.zip ./imported \
  --profile polygon-compatible
```

Run `reporch <command> --help` for every option. `reporch.yaml` is the source of
truth, and compatibility/package export commands compile it in memory without
replacing the last immutable `reporch.problem.json` baseline. Pass an immutable
JSON manifest instead when reproducing an existing commit. Compatibility
commands report unsupported or lossy features instead of silently discarding
them. After an approved release is built, publication is always explicit:
`reporch publication publish` asks for confirmation, or requires `--yes` in CI.
`reporch publish` is the shorter equivalent for the linked project's latest
ready release.

Shell completion scripts are generated from the exact installed command tree:

```bash
# zsh
reporch completion zsh > "${fpath[1]}/_reporch"

# bash
reporch completion bash > ~/.local/share/bash-completion/completions/reporch

# fish
reporch completion fish > ~/.config/fish/completions/reporch.fish
```

Immutable releases have a separate, scriptable lifecycle:

```bash
reporch release build
reporch release list --format json
reporch release show --release-id <uuid>
reporch release download --release-id <uuid> --output problem.zip
```

Official validation history is available without copying a project UUID from
Studio when the current directory is linked:

```bash
reporch validation list
reporch validation show --validation-run-id <uuid>
reporch validation watch --validation-run-id <uuid>
```

Immutable revisions can be compared or restored without overwriting the current
working tree. Restore always creates a new checkout and downloads bytes from the
commit-bound CAS descriptors, not from the mutable project file view:

```bash
reporch revision diff <from-commit> <to-commit>
reporch revision restore <commit> --directory ../restored-problem
```

Local validation toolchains are opt-in. The available catalog is embedded in
the binary and verified with an embedded Minisign public key before it is
parsed. Install accepts only a catalog ID; arbitrary tags and images are not an
input surface. Every catalog image is pinned by SHA-256 and the OCI runtime is
re-inspected after the explicit pull:

```bash
reporch toolchain list
reporch toolchain inspect gcc-16.1-cpp
reporch toolchain install gcc-16.1-cpp
```

`sandbox run` continues to use `--pull=never`, network isolation, a read-only
root filesystem, dropped capabilities, and resource limits. Local results are
never accepted as Studio release evidence.

Downloads never overwrite an existing path and are installed only after the
declared size and SHA-256 both match. Progress events can be resumed by durable
cursor. Use JSONL for an unbounded stream, or bound JSON output for CI:

```bash
reporch --format jsonl events watch --cursor 42
reporch --format json events watch --max-events 10
```

## Authentication and privacy

The CLI is an OAuth public client. It opens Reporch's Device Authorization flow
in the system browser and stores refresh credentials only in the operating
system credential store. It contains no OAuth client secret, does not read web
browser cookies, and does not write tokens to project files. Plain HTTP is
rejected except for an explicitly enabled loopback development issuer.

The CLI sends requests only when you run authentication or remote project
commands. It has no analytics or telemetry.

For separate production and development endpoints, create the user-only
`config.toml` under `$REPORCH_CONFIG_HOME`, macOS Application Support,
`$XDG_CONFIG_HOME/reporch`, or Windows AppData:

```toml
version = 1

[profiles.production]
studio_api_url = "https://studio.reporch.com"
oidc_issuer = "https://reporch.com/oauth"
cli_client_id = "reporch-studio-cli"
studio_web_url = "https://studio.reporch.com"
allow_insecure_http = false
```

Select it with `reporch --profile production doctor`. Explicit flags override
environment variables, and environment variables override profile values. The
file must be regular, bounded, and neither it nor its directory may be group-
or world-writable. Project files cannot override API or OAuth endpoints, and
profiles never contain tokens.

## Local sandbox

User code is never executed by the CLI unless you explicitly invoke
`reporch sandbox run`. Local execution requires rootless Podman or rootless
Docker, a digest-pinned image, disabled networking, a read-only project mount,
and explicit resource limits.

## Build from source

Rust 1.96.0 or newer is required.

```bash
cargo build --locked --release -p reporch-cli
./target/release/reporch --version
```

Run the complete local verification suite:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
npm test
```

`npm test` also verifies the checksum and required review-pool surface of the
pinned Studio OpenAPI artifact. Contract drift therefore fails before a CLI
release can be packaged.

## Release integrity

Official npm packages and standalone native archives are built on
GitHub-hosted runners for each supported OS. Every uploaded asset is covered by
`SHA256SUMS` and GitHub artifact provenance; the release also includes an SPDX
SBOM and machine-readable npm and native manifests. Unix archives normalize
ordering, timestamps, ownership, and gzip metadata so rebuilds are byte-for-byte
comparable. npm packages contain no `preinstall`, `install`, or `postinstall`
scripts.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License 2.0](LICENSE). The license does not grant
permission to use Reporch trademarks, service marks, or logos.
