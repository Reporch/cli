# Reporch Runtime v1

> RC8 implementation contract. The native VM path is not the default until all
> five platform qualification gates pass and the RC8 release is published.

Once the qualification gates above pass, Reporch Runtime becomes the default
local execution boundary for Reporch CLI. It
boots an ephemeral Linux virtual machine through Apple Virtualization.framework
on macOS, Firecracker/KVM on Linux, or Hyper-V/HCS on Windows. Docker and Podman
are explicit deprecated compatibility backends and are never auto-discovered.

## Trust and installation

The CLI embeds the Minisign public trust root used for the runtime channel. A
target-specific manifest is verified before JSON parsing and binds every
kernel, root filesystem, guest agent, host service, and virtual-machine monitor
artifact by exact byte size and SHA-256. Manifests include an expiring validity
window, monotonic sequence, target, backend, and guest protocol range.

Downloads use credential-free HTTPS, bounded metadata bodies, stalled-transfer
timeouts, total deadlines, and create-new staging files. A complete bundle is
renamed into place before `current.json` changes. The previous installation is
retained for rollback, and repair/reset never accepts a lower sequence.

Native installers carry the base runtime. npm and standalone archives do not
rely on lifecycle scripts: the first operational command must finish the same
signed bootstrap before it proceeds. `--help`, `--version`, and shell completion
remain immediate. Language toolchains are installed automatically on first use;
`toolchain prefetch` exists for CI and offline preparation.

## Isolation

- No guest network device.
- No host project, home-directory, keychain, or credential mount.
- Read-only base and signed ext4/VHDX toolchain block devices. A bounded,
  disposable overlay supplies only guest mount points and never mutates the
  signed image.
- Disposable work storage and one VM per execution.
- Non-root guest workload identity.
- Host and guest CPU, memory, PID, wall-clock, output, and artifact limits.
- Nonce-bound, versioned job/result protocol with content-addressed inputs.
- Bounded cleanup for success, failure, timeout, cancellation, and guest crash.

Local output is advisory. Only Studio validation creates publication evidence.

## Unsupported virtualization

`runtime status` reports `remote_only` when KVM, Hyper-V, or Apple hardware
virtualization cannot be used. Authoring and static `check` remain available.
TTY execution asks once before using a private, expiring Studio preview and
records consent per account/profile. `--no-input` requires
`--allow-remote-fallback` or `REPORCH_ALLOW_REMOTE_FALLBACK=1`; otherwise it
creates no network request and exits with the stable
`runtime.remote_fallback_not_allowed` error.

## Stable errors

Runtime failures use the `reporch.cli-error.v1` envelope and the
`runtime.*` codes documented in the CLI contract. Authentication, quota,
conflict, cancellation, and infrastructure exit codes remain unchanged.
