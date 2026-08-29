import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const ROOT = resolve(import.meta.dirname, "..");

async function executable(path, contents) {
  await writeFile(path, contents);
  await chmod(path, 0o755);
}

test("resumable toolchain release keeps complete per-entry checkpoints", async (context) => {
  const temporary = await mkdtemp(join(tmpdir(), "reporch-toolchain-resume-"));
  context.after(() => rm(temporary, { recursive: true, force: true }));
  const repository = join(temporary, "repository");
  const scripts = join(repository, "scripts");
  const runtime = join(repository, "runtime");
  const fakeBin = join(temporary, "bin");
  await mkdir(scripts, { recursive: true });
  await mkdir(runtime, { recursive: true });
  await mkdir(fakeBin, { recursive: true });
  await mkdir(join(repository, "target", "release"), { recursive: true });
  await writeFile(
    join(runtime, "toolchains.lock.json"),
    `${JSON.stringify({ entries: Array.from({ length: 12 }, (_, index) => ({ id: `tool-${index}` })) })}\n`
  );
  await executable(
    join(scripts, "build-toolchain-release-resumable.sh"),
    await readFile(join(ROOT, "scripts", "build-toolchain-release-resumable.sh"), "utf8")
  );
  await executable(
    join(scripts, "build-toolchain-candidates.sh"),
    `#!/bin/sh
set -eu
output=$2
id=$3
count_file=$FAKE_BUILD_COUNT
count=0
test ! -f "$count_file" || count=$(cat "$count_file")
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
if [ "\${FAKE_FAIL_AT:-}" = "$count" ]; then exit 9; fi
mkdir "$output"
for name in \
  "$id-linux-arm64.ext4.zst" "$id-linux-arm64.ext4.zst.build.json" \
  "$id-linux-arm64.source.spdx.json" \
  "$id-linux-x64.ext4.zst" "$id-linux-x64.ext4.zst.build.json" \
  "$id-linux-x64.source.spdx.json" \
  "$id-windows-x64.vhdx.zst" "$id-windows-x64.vhdx.zst.build.json"; do
  printf '%s\n' "$id:$name" > "$output/$name"
done
`
  );
  await executable(
    join(scripts, "compare-toolchain-entry.mjs"),
    `import { readFileSync } from "node:fs";
const values = process.argv.slice(2);
if (values[0] === "--aggregate") {
  const entries = readFileSync(values[1], "utf8").trim().split("\\n").map(JSON.parse);
  process.stdout.write(JSON.stringify({schema:"reporch.toolchain-reproducibility.v2",toolchains:entries.length,entries}) + "\\n");
} else {
  const id = values[2];
  process.stdout.write(JSON.stringify({schema:"reporch.toolchain-entry-reproducibility.v2",id,files:9,bytes:1,tree_sha256:"a".repeat(64)}) + "\\n");
}
`
  );
  await executable(join(fakeBin, "cargo"), "#!/bin/sh\nexit 0\n");
  await executable(
    join(repository, "target", "release", "reporch-toolchain-release-builder"),
    `#!/bin/sh
set -eu
artifacts=$2
index=$5
for archive in "$artifacts"/*.zst; do
  printf '%s\n' evidence > "$archive.spdx.json"
  printf '%s\n' evidence > "$archive.intoto.jsonl"
done
printf '%s\n' '{"schema":"reporch.toolchain-index.v2"}' > "$index"
`
  );
  const git = (args) => spawnSync("git", args, { cwd: repository, encoding: "utf8" });
  assert.equal(git(["init", "-q"]).status, 0);
  assert.equal(git(["config", "user.email", "test@reporch.com"]).status, 0);
  assert.equal(git(["config", "user.name", "Reporch Test"]).status, 0);
  assert.equal(git(["add", "."]).status, 0);
  assert.equal(git(["commit", "-qm", "fixture"]).status, 0);

  const source = join(temporary, "source");
  const checkpoint = join(temporary, "checkpoint");
  const count = join(temporary, "build-count");
  await mkdir(source);
  const run = (suffix, failAt = "") =>
    spawnSync(
      "sh",
      [
        "scripts/build-toolchain-release-resumable.sh",
        source,
        checkpoint,
        join(temporary, `candidates-${suffix}`),
        join(temporary, `report-${suffix}.json`)
      ],
      {
        cwd: repository,
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${fakeBin}:${process.env.PATH}`,
          FAKE_BUILD_COUNT: count,
          FAKE_FAIL_AT: failAt
        }
      }
    );

  const interrupted = run("interrupted", "3");
  assert.equal(interrupted.status, 9, interrupted.stderr + interrupted.stdout);
  const resumed = run("resumed");
  assert.equal(resumed.status, 0, resumed.stderr + resumed.stdout);
  assert.equal(Number((await readFile(count, "utf8")).trim()), 25);
  const reused = run("reused");
  assert.equal(reused.status, 0, reused.stderr + reused.stdout);
  assert.equal(Number((await readFile(count, "utf8")).trim()), 25);
  assert.match(reused.stdout, /reusing complete primary toolchain checkpoint/);
  assert.match(reused.stdout, /reusing verified reproducibility evidence/);
});
