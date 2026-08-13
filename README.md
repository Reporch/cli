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

## Start a problem

```bash
reporch auth login
reporch project create --title "My problem" --directory ./my-problem
cd my-problem
reporch statement add --locale ko --path statement.md --title "My problem"
reporch test                         # interactive, line-oriented guide
reporch solution add --name reference --source solutions/reference.cpp \
  --language cpp --expected accepted
reporch check                        # completely offline
reporch submit                       # push, Studio verification, review request
```

`reporch.yaml` is the human-editable source. `reporch.problem.json` is generated
only after the server has assigned the immutable commit ID and bound every file
hash. UUIDs, SHA-256 values, and manifest internals never need to be entered by
the author.

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
reporch manifest compatibility reporch.problem.json \
  --profile icpc202509 --strict

reporch package export reporch.problem.json problem.zip \
  --profile domjudge-zip

reporch package import polygon-package.zip ./imported \
  --profile polygon-compatible
```

Run `reporch <command> --help` for every option. `reporch.yaml` is the source of
truth; compatibility commands report unsupported or lossy features instead of
silently discarding them. After an approved release is built, publication is
always explicit: `reporch publication publish` asks for confirmation, or
requires `--yes` in CI.

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

Official npm releases are built on GitHub-hosted runners for each supported OS.
The release workflow creates npm provenance, GitHub artifact attestations,
SHA-256 checksums, and an SPDX SBOM. npm packages contain no `preinstall`,
`install`, or `postinstall` scripts.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License 2.0](LICENSE). The license does not grant
permission to use Reporch trademarks, service marks, or logos.
