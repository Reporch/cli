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
reporch manifest validate reporch.problem.json
reporch project push --manifest reporch.problem.json --message "Initial version"
reporch project validate --project-id <project-id> --commit-id <commit-id>
reporch review submit \
  --project-id <project-id> \
  --commit-id <commit-id> \
  --validation-run-id <validation-run-id>
```

An independent project reviewer completes the digest-bound approval before a
release package can be built:

```bash
reporch review list --project-id <project-id>
reporch review approve --project-id <project-id> --review-id <review-id>
reporch project package \
  --project-id <project-id> \
  --commit-id <commit-id> \
  --validation-run-id <validation-run-id> \
  --output problem.zip
```

The server rejects self-approval: the commit author and final reviewer must be
different Reporch subjects, and every approval is bound to the exact commit,
validation run, manifest digest, and reviewer entitlement version.

Create a completely local project without signing in:

```bash
reporch project init \
  --title "My problem" \
  --problem-type standard \
  --directory ./my-problem
```

The available problem types are `standard`, `scored`, `interactive`,
`output-only`, `library`, and `grader`.

## Package compatibility

```bash
reporch manifest compatibility reporch.problem.json \
  --profile icpc202509 --strict

reporch package export reporch.problem.json problem.zip \
  --profile domjudge-zip

reporch package import polygon-package.zip ./imported \
  --profile polygon-compatible
```

Run `reporch <command> --help` for every option. The native Reporch manifest is
the source of truth; compatibility commands report unsupported or lossy
features instead of silently discarding them.

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

## Release integrity

Official npm releases are built on GitHub-hosted runners for each supported OS.
The release workflow creates npm provenance, GitHub artifact attestations,
SHA-256 checksums, and an SPDX SBOM. npm packages contain no `preinstall`,
`install`, or `postinstall` scripts.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License 2.0](LICENSE). The license does not grant
permission to use Reporch trademarks, service marks, or logos.
