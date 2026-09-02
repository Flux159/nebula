# install-image acceptance on Windows/NTFS (issue #24).
#
# The bash suite covers macOS and Linux; this exists because NTFS is the only
# filesystem that will NOT make a hole on a plain seek — it allocates the
# zeros unless the handle was marked sparse with FSCTL_SET_SPARSE first. So
# "is the output sparse" is a real question here, not a formality, and it is
# the platform where the cost showed up: every written byte goes through
# Defender's real-time scanner.
#
# Runs in a throwaway NEBULA_HOME under $env:TEMP; never touches ~\.nebula.
$ErrorActionPreference = "Continue"
Set-Location (Split-Path -Parent $PSScriptRoot)

$nebula = ".\target\debug\nebula.exe"
$H   = Join-Path $env:TEMP "nebula-img-test"
$SRC = Join-Path $env:TEMP "nebula-img-src"

$script:Pass = 0
$script:Fail = 0
function Check($name, [scriptblock]$test) {
    $ok = $false
    try { $ok = [bool](& $test) } catch { $ok = $false }
    if ($ok) { Write-Host "PASS: $name"; $script:Pass++ }
    else     { Write-Host "FAIL: $name"; $script:Fail++ }
}
function Sha($p) { (Get-FileHash -Algorithm SHA256 $p).Hash }
function LogicalMB($p) { [math]::Round((Get-Item $p).Length / 1MB) }

# Size on disk. NTFS reports allocated ranges through fsutil; summing them is
# the only way to see whether a hole was really punched.
function AllocatedMB($p) {
    $out = & fsutil sparse queryrange $p 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $out) {
        # Not sparse at all: everything is allocated.
        return (LogicalMB $p)
    }
    $total = 0L
    foreach ($line in $out) {
        if ($line -match 'Allocated ranges?.*length:\s*(0x[0-9a-fA-F]+|\d+)') {
            $total += [convert]::ToInt64($matches[1].Replace('0x',''), 16)
        } elseif ($line -match 'length:\s*(0x[0-9a-fA-F]+)') {
            $total += [convert]::ToInt64($matches[1].Replace('0x',''), 16)
        }
    }
    return [math]::Round($total / 1MB)
}
function IsSparse($p) {
    $flag = & fsutil sparse queryflag $p 2>$null
    return ($flag -match 'is set')
}

if (Test-Path $H) { Remove-Item -Recurse -Force $H }
if (Test-Path $SRC) { Remove-Item -Recurse -Force $SRC }
New-Item -ItemType Directory -Force -Path $H, $SRC | Out-Null

# Ports of its own: nebulad refuses to share them with a running instance.
@"
api_port = 7561
dns_port = 42173
k8s_port = 6563
dns_zone = "imgtest.local"
max_ram_mib = 2048
cpus = 2
data_disk_gib = 8
"@ | Set-Content -Path (Join-Path $H "config.toml") -Encoding ascii

$K_SRC = Join-Path $env:USERPROFILE ".nebula\kernel\Image"
$R_SRC = Join-Path $env:USERPROFILE ".nebula\images\rootfs-pristine.img"
if ($env:KERNEL_SRC) { $K_SRC = $env:KERNEL_SRC }
if ($env:ROOTFS_SRC) { $R_SRC = $env:ROOTFS_SRC }
if (-not (Test-Path $K_SRC) -or -not (Test-Path $R_SRC)) {
    Write-Host "FATAL: no guest images at $K_SRC / $R_SRC"; exit 1
}
$LOG = LogicalMB $R_SRC
Write-Host "source kernel: $K_SRC"
Write-Host "source rootfs: $R_SRC ($LOG MiB logical)"
$K_SHA = Sha $K_SRC
$R_SHA = Sha $R_SRC

Write-Host ""
Write-Host "--- raw (uncompressed) sources"
$env:NEBULA_HOME = $H
$t = Measure-Command { & $nebula install-image --kernel $K_SRC --rootfs $R_SRC | Out-Null }
Check "install-image succeeds"        { Test-Path (Join-Path $H "disks\rootfs.img") }
Check "kernel is byte-identical"      { (Sha (Join-Path $H "kernel\Image")) -eq $K_SHA }
Check "pristine is byte-identical"    { (Sha (Join-Path $H "images\rootfs-pristine.img")) -eq $R_SHA }
Check "live disk is byte-identical"   { (Sha (Join-Path $H "disks\rootfs.img")) -eq $R_SHA }
Check "no staging copy left behind"   { -not (Test-Path (Join-Path $H "cache\image-install")) }

$pristine = Join-Path $H "images\rootfs-pristine.img"
$live     = Join-Path $H "disks\rootfs.img"
$pAlloc = AllocatedMB $pristine
$lAlloc = AllocatedMB $live
Write-Host ("    pristine: {0} MiB allocated of {1} MiB logical (install took {2:N1}s)" -f $pAlloc, $LOG, $t.TotalSeconds)
Write-Host ("    live:     {0} MiB allocated" -f $lAlloc)
Check "NTFS sparse flag is set"       { IsSparse $pristine }
Check "pristine is sparse on NTFS"    { $pAlloc -lt ($LOG / 4) }
Check "live disk is sparse on NTFS"   { $lAlloc -lt ($LOG / 4) }

Write-Host ""
Write-Host "--- gzip sources (what embedders actually ship)"
$kgz = Join-Path $SRC "kernel-Image.gz"
$rgz = Join-Path $SRC "rootfs.img.gz"
foreach ($pair in @(@($K_SRC, $kgz), @($R_SRC, $rgz))) {
    $in  = [System.IO.File]::OpenRead($pair[0])
    $out = [System.IO.File]::Create($pair[1])
    $gz  = New-Object System.IO.Compression.GZipStream($out, [System.IO.Compression.CompressionLevel]::Optimal)
    $in.CopyTo($gz); $gz.Dispose(); $out.Dispose(); $in.Dispose()
}
Remove-Item -Recurse -Force $H
New-Item -ItemType Directory -Force -Path $H | Out-Null
@"
api_port = 7561
dns_port = 42173
k8s_port = 6563
dns_zone = "imgtest.local"
max_ram_mib = 2048
cpus = 2
data_disk_gib = 8
"@ | Set-Content -Path (Join-Path $H "config.toml") -Encoding ascii
$t = Measure-Command { & $nebula install-image --kernel $kgz --rootfs $rgz | Out-Null }
Check "install from .gz succeeds"     { Test-Path $live }
Check "kernel from .gz is identical"  { (Sha (Join-Path $H "kernel\Image")) -eq $K_SHA }
Check "pristine from .gz is identical"{ (Sha $pristine) -eq $R_SHA }
Check "live from .gz is identical"    { (Sha $live) -eq $R_SHA }
$pAlloc = AllocatedMB $pristine
Write-Host ("    pristine: {0} MiB allocated of {1} MiB logical (install took {2:N1}s)" -f $pAlloc, $LOG, $t.TotalSeconds)
Check "gz install is sparse"          { $pAlloc -lt ($LOG / 4) }

Write-Host ""
Write-Host "--- upgrade: installing over an existing install leaves nothing of the old one"
foreach ($p in @($pristine, $live)) {
    $fs = [System.IO.File]::Open($p, 'Open', 'Write')
    $fs.Seek(400MB, 'Begin') | Out-Null
    $bytes = [System.Text.Encoding]::ASCII.GetBytes(("STALE" * 1000))
    $fs.Write($bytes, 0, $bytes.Length); $fs.Dispose()
}
Check "scribble took effect"          { (Sha $pristine) -ne $R_SHA }
& $nebula install-image --kernel $kgz --rootfs $rgz | Out-Null
Check "reinstall restores pristine"   { (Sha $pristine) -eq $R_SHA }
Check "reinstall restores live disk"  { (Sha $live) -eq $R_SHA }

Write-Host ""
Write-Host "--- upgrade: a genuinely different image replaces the old one wholesale"
$other = Join-Path $SRC "other.img"
$fs = [System.IO.File]::Create($other)
$bytes = [System.Text.Encoding]::ASCII.GetBytes(("OTHERIMAGE" * 100))
$fs.Write($bytes, 0, $bytes.Length)
$fs.Seek(200MB, 'Begin') | Out-Null
$tail = [System.Text.Encoding]::ASCII.GetBytes("tail-marker")
$fs.Write($tail, 0, $tail.Length)
$fs.SetLength(300MB); $fs.Dispose()
$O_SHA = Sha $other
& $nebula install-image --kernel $K_SRC --rootfs $other | Out-Null
Check "different image installs clean" { (Sha $pristine) -eq $O_SHA }
Check "live disk matches new image"    { (Sha $live) -eq $O_SHA }
Check "size shrank to the new image"   { (Get-Item $pristine).Length -eq (Get-Item $other).Length }

Write-Host ""
Write-Host "--- vessels reset (the other clone_file path)"
& $nebula install-image --kernel $K_SRC --rootfs $R_SRC | Out-Null
$fs = [System.IO.File]::Open($live, 'Open', 'Write')
$fs.Seek(400MB, 'Begin') | Out-Null
$bytes = [System.Text.Encoding]::ASCII.GetBytes(("BROKEN" * 1000))
$fs.Write($bytes, 0, $bytes.Length); $fs.Dispose()
& $nebula vessels reset engine | Out-Null
Check "reset restores pristine bytes"  { (Sha $live) -eq $R_SHA }
Check "reset output is sparse"         { (AllocatedMB $live) -lt ($LOG / 4) }

if ($env:SKIP_BOOT -ne "1") {
    Write-Host ""
    Write-Host "--- the installed image actually boots"
    # Start-Process, not Win32_Process::Create: the latter does not inherit
    # this shell's environment, so the daemon would boot the developer's real
    # ~\.nebula instead of the scratch home and every check below would be
    # measuring the wrong engine (it did, the first time).
    #
    # Windows OpenSSH kills a session's process tree on disconnect, so run
    # this script itself detached over ssh (Win32_Process::Create) — then its
    # children survive too.
    $exe = (Resolve-Path ".\target\debug\nebulad.exe").Path
    Start-Process -FilePath $exe -WindowStyle Hidden | Out-Null
    $booted = $false
    foreach ($i in 1..240) {
        $st = & $nebula status 2>&1 | Out-String
        if ($st -match "agent:.*healthy") { $booted = $true; break }
        Start-Sleep -Milliseconds 500
    }
    Check "engine boots from the sparse image" { $booted }
    # Read a real file out of the guest. Note the marker must not appear in
    # the command itself: a failing native command's error record echoes the
    # command line back, which will happily "match" a naive pattern.
    Check "guest filesystem is intact" {
        if (-not $booted) { return $false }
        $out = & $nebula exec sh -c "test -x /sbin/nebula-init && echo GUESTFS_OK" 2>&1 | Out-String
        $out -match "GUESTFS_OK"
    }
    & $nebula down 2>&1 | Out-Null
}

Write-Host ""
Write-Host "image-install: $($script:Pass) passed, $($script:Fail) failed"
if ($script:Fail -ne 0) { exit 1 }
