param(
  [Parameter(Mandatory = $true)][string]$ReporchBinary,
  [Parameter(Mandatory = $true)][string]$EvidenceDirectory
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-True([bool]$Condition, [string]$Message) {
  if (-not $Condition) { throw $Message }
}

$binary = [System.IO.Path]::GetFullPath($ReporchBinary)
$evidence = [System.IO.Path]::GetFullPath($EvidenceDirectory)
Assert-True ([System.IO.Path]::IsPathFullyQualified($binary)) "qualification binary path must be absolute"
Assert-True ([System.IO.Path]::IsPathFullyQualified($evidence)) "evidence path must be absolute"
$binaryItem = Get-Item -LiteralPath $binary -Force
Assert-True (-not $binaryItem.PSIsContainer -and -not ($binaryItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) "qualification binary must be a regular non-reparse file"
if (Test-Path -LiteralPath $evidence) {
  $evidenceItem = Get-Item -LiteralPath $evidence -Force
  Assert-True ($evidenceItem.PSIsContainer -and -not ($evidenceItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) "evidence path must be a non-reparse directory"
} else {
  New-Item -ItemType Directory -Path $evidence | Out-Null
}

$service = Get-CimInstance Win32_Service -Filter "Name='ReporchRuntime'"
Assert-True ($null -ne $service -and $service.State -eq "Running") "ReporchRuntime service is not running"
$serviceProcess = Get-Process -Id ([int]$service.ProcessId)
$coldHandles = $serviceProcess.HandleCount
$hcsdiag = Join-Path $env:SystemRoot "System32\hcsdiag.exe"
Assert-True (Test-Path -LiteralPath $hcsdiag -PathType Leaf) "hcsdiag.exe is required for exact HCS orphan detection"

function Get-HcsSystems {
  $output = & $hcsdiag list 2>&1 | Out-String
  if ($LASTEXITCODE -ne 0) { throw "hcsdiag list failed: $output" }
  return [regex]::Matches($output, '(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b') |
    ForEach-Object { $_.Value.ToLowerInvariant() } | Sort-Object -Unique
}

$statusText = & $binary --format json runtime status 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { throw "runtime status failed: $statusText" }
$statusText | Set-Content -LiteralPath (Join-Path $evidence "status.json") -Encoding utf8NoBOM
$status = $statusText | ConvertFrom-Json
Assert-True ($status.schema -eq "reporch.cli-result.v1" -and $status.command -eq "runtime status") "runtime status envelope is invalid"
Assert-True ($status.data.installed_sequence -eq 24) "runtime status selected the wrong signed runtime sequence"
Assert-True ($status.data.target -eq "windows-x64-msvc" -and $status.data.backend -eq "hyper_v_hcs") "runtime status selected the wrong Windows backend"
Assert-True ($status.data.availability -eq "ready" -and $status.data.virtualization_available -and $status.data.service_available) "Windows native runtime is not ready"

# HCS, Winsock, and the signed toolchain verifier intentionally retain a
# bounded set of process-wide initialization handles. Establish the leak
# baseline after one complete lifecycle so the 100-iteration gate measures
# sustained growth instead of cold-start initialization.
$warmupText = & $binary --format json runtime qualification --iterations 1 --toolchain bash-5.3 2>&1 | Out-String
$warmupText | Set-Content -LiteralPath (Join-Path $evidence "warmup.json") -Encoding utf8NoBOM
if ($LASTEXITCODE -ne 0) { throw "runtime qualification warmup failed: $warmupText" }
$warmup = $warmupText | ConvertFrom-Json
Assert-True ($warmup.schema -eq "reporch.cli-result.v1" -and $warmup.command -eq "runtime qualification") "runtime qualification warmup envelope is invalid"
Assert-True ($warmup.data.iterations -eq 1 -and $warmup.data.passed) "runtime qualification warmup was incomplete"

$warmedService = Get-CimInstance Win32_Service -Filter "Name='ReporchRuntime'"
Assert-True ($null -ne $warmedService -and $warmedService.State -eq "Running" -and [int]$warmedService.ProcessId -eq [int]$service.ProcessId) "runtime service restarted during warmup"
$beforeHandles = (Get-Process -Id ([int]$service.ProcessId)).HandleCount
$beforeSystems = @(Get-HcsSystems)
$beforeSystems | Set-Content -LiteralPath (Join-Path $evidence "before-hcs-systems.txt") -Encoding utf8NoBOM

$qualificationText = & $binary --format json runtime qualification --iterations 100 --toolchain bash-5.3 2>&1 | Out-String
$qualificationText | Set-Content -LiteralPath (Join-Path $evidence "qualification.json") -Encoding utf8NoBOM
if ($LASTEXITCODE -ne 0) { throw "runtime qualification failed: $qualificationText" }
$qualification = $qualificationText | ConvertFrom-Json
Assert-True ($qualification.schema -eq "reporch.cli-result.v1" -and $qualification.command -eq "runtime qualification") "runtime qualification envelope is invalid"
$result = $qualification.data
Assert-True ($result.schema -eq "reporch.native-runtime-qualification.v1") "runtime qualification result schema is invalid"
Assert-True ($result.target -eq "windows-x64-msvc" -and $result.backend -eq "hyper_v_hcs") "runtime qualification used the wrong backend"
Assert-True ($result.iterations -eq 100 -and $result.p95_ms -le 5000) "Windows native VM performance gate failed"
Assert-True ($result.lifecycle -and $result.handshake -and $result.guest_workload -and $result.cleanup -and $result.signed_toolchain_unchanged -and $result.passed) "Windows native VM qualification was incomplete"

Start-Sleep -Milliseconds 500
$afterSystems = @(Get-HcsSystems)
$afterSystems | Set-Content -LiteralPath (Join-Path $evidence "after-hcs-systems.txt") -Encoding utf8NoBOM
Assert-True (($beforeSystems -join "`n") -eq ($afterSystems -join "`n")) "an HCS system was leaked by qualification"
$jobsRoot = Join-Path $env:ProgramData "Reporch\Runtime\jobs"
if (Test-Path -LiteralPath $jobsRoot) {
  $leakedJobs = @(Get-ChildItem -LiteralPath $jobsRoot -Force)
  Assert-True ($leakedJobs.Count -eq 0) "a Windows runtime job directory was leaked"
}
$afterHandles = (Get-Process -Id ([int]$service.ProcessId)).HandleCount
$handleCounts = [ordered]@{
  schema = "reporch.windows-service-handle-counts.v1"
  cold = $coldHandles
  baseline_after_warmup = $beforeHandles
  after_100_iterations = $afterHandles
  sustained_growth = $afterHandles - $beforeHandles
}
$handleCounts | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence "handle-counts.json") -Encoding utf8NoBOM
Assert-True ($afterHandles -le $beforeHandles + 16) "runtime service handle count grew unexpectedly"

$hostResult = [ordered]@{
  schema = "reporch.windows-vm-host-qualification.v1"
  completed_at = [DateTime]::UtcNow.ToString("yyyy-MM-ddTHH:mm:ssZ")
  target = "windows-x64-msvc"
  backend = "hyper_v_hcs"
  service_pid = [int]$service.ProcessId
  cold_service_handles = $coldHandles
  before_service_handles = $beforeHandles
  after_service_handles = $afterHandles
  orphan_hcs_systems = 0
  orphan_job_directories = 0
  passed = $true
}
$hostResult | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $evidence "host-result.json") -Encoding utf8NoBOM
$checksums = Get-ChildItem -LiteralPath $evidence -File | Sort-Object Name | ForEach-Object {
  $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
  "$hash  $($_.Name)"
}
$checksums | Set-Content -LiteralPath (Join-Path $evidence "SHA256SUMS") -Encoding ascii
$hostResult | ConvertTo-Json -Depth 4
