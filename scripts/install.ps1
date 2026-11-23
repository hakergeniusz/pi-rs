# scripts/install.ps1 - per-user installer for the pi-rs Windows release binary.
#
# ONE-LINER (PowerShell 5.1+ or 7+, no admin rights required):
#
#   irm https://raw.githubusercontent.com/hakergeniusz/pi-rs/main/scripts/install.ps1 | iex
#
#   # To pass options, download the script first so you can read it, then run:
#   #   irm https://raw.githubusercontent.com/hakergeniusz/pi-rs/main/scripts/install.ps1 -OutFile install.ps1
#   #   powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version v0.1.1 -Force -AddToPath
#
# SAFETY MODEL:
#   * This script is the ONLY downloaded content ever passed to iex. The
#     release binary and checksum are fetched with Invoke-WebRequest
#     -UseBasicParsing into a scratch directory and are never executed,
#     sourced, or piped into any shell.
#   * SHA-256 is verified with Get-FileHash against the published
#     pi-windows-x86_64.exe.sha256 BEFORE anything is written to the prefix.
#     A mismatch aborts with the prefix untouched.
#   * All installs write ONLY inside -Prefix, apart from the scratch directory
#     under the system temp path (removed on exit), and the opt-in -AddToPath
#     switch which appends the prefix to the *user* PATH (never machine PATH).
#   * No admin rights, no machine-wide writes, no services, no registry writes
#     other than the user PATH when -AddToPath is given.
#   * x86-64 (AMD64) only; any other architecture aborts with a clear message.

param(
  # Release tag to install. Use 'latest' for the newest release.
  [string]$Version = 'v0.1.1',
  # Install directory. Defaults to the per-user Programs folder.
  [string]$Prefix = '',
  # Replace an existing $Prefix\pi-rs.exe instead of refusing.
  [switch]$Force,
  # Opt-in: append $Prefix to the current user's PATH (not the machine PATH).
  [switch]$AddToPath
)

$ErrorActionPreference = 'Stop'

$Repo             = 'hakergeniusz/pi-rs'
$Asset            = 'pi-windows-x86_64.exe'
$SumAsset         = "$Asset.sha256"
$TargetName       = 'pi-rs.exe'
$InstallerVersion = '1.0.0'

function Fail {
  param([string]$Message)
  throw "install.ps1: error: $Message"
}

# --------------------------------------------------------------- arguments
$Version = if ($null -eq $Version) { '' } else { $Version.Trim() }
if ([string]::IsNullOrEmpty($Version)) { $Version = 'v0.1.1' }
if ($Version -ieq 'latest') {
  $downloadBase = "https://github.com/$Repo/releases/latest/download"
} else {
  if ($Version -notmatch '^v') { $Version = "v$Version" }
  $downloadBase = "https://github.com/$Repo/releases/download/$Version"
}
$binaryUrl = "$downloadBase/$Asset"
$sumUrl    = "$downloadBase/$SumAsset"

if ([string]::IsNullOrWhiteSpace($Prefix)) {
  $localAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA')
  if ([string]::IsNullOrWhiteSpace($localAppData)) {
    Fail "LOCALAPPDATA is not set; pass -Prefix explicitly."
  }
  $Prefix = Join-Path $localAppData 'Programs\pi-rs'
}
$Prefix = [System.IO.Path]::GetFullPath($Prefix)

# ------------------------------------------------------------ architecture
# PROCESSOR_ARCHITECTURE is the native architecture of the OS/process;
# PROCESSOR_ARCHITEW6432 is set when a 32-bit PowerShell runs on 64-bit
# Windows. Either one reporting AMD64 means we can run the x86-64 binary.
$arch = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE')
$wowArch = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITEW6432')
if (-not [string]::IsNullOrEmpty($wowArch)) { $arch = $wowArch }
if ($arch -ne 'AMD64' -and $arch -ne 'x86_64') {
  Fail "unsupported CPU architecture '$arch': only x86_64 (AMD64) Windows is supported."
}

# --------------------------------------------------------------- scratch dir
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("pi-rs-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$binTmp = Join-Path $work $Asset
$sumTmp = Join-Path $work $SumAsset

try {
  Write-Host "install.ps1 v${InstallerVersion}: fetching $Asset for Windows/$arch (release $Version)"

  # ------------------------------------------------------------- download
  try {
    Invoke-WebRequest -UseBasicParsing -Uri $binaryUrl -OutFile $binTmp
  } catch {
    Fail "could not download '$binaryUrl': $($_.Exception.Message)"
  }
  if (-not (Test-Path -LiteralPath $binTmp) -or (Get-Item -LiteralPath $binTmp).Length -eq 0) {
    Fail 'downloaded binary is empty or missing'
  }

  try {
    Invoke-WebRequest -UseBasicParsing -Uri $sumUrl -OutFile $sumTmp
  } catch {
    Fail "could not download '$sumUrl': $($_.Exception.Message)"
  }

  # ------------------------------------------------------------- checksum
  # CI writes `<sha>  <basename>` with a trailing LF (two spaces, lowercase
  # hex). Be tolerant of CRLF and of a bare digest on a single line.
  $sumRaw = Get-Content -LiteralPath $sumTmp -Raw
  $sumLine = ($sumRaw -split "`r?`n") | Where-Object { $_ -match '\S' } | Select-Object -First 1
  if (-not $sumLine) { Fail "checksum file '$SumAsset' is empty" }

  $parts = $sumLine.Trim() -split '\s+', 2
  $expectedHash = $parts[0].ToLowerInvariant()
  $expectedName = if ($parts.Count -ge 2) { $parts[1].Trim() } else { '' }

  if ($expectedHash -notmatch '^[0-9a-f]{64}$') {
    Fail "unexpected checksum format in '$SumAsset': $sumLine"
  }
  if ($expectedName -and $expectedName -ne $Asset) {
    Fail "checksum file names '$expectedName' but expected '$Asset'"
  }

  $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $binTmp).Hash.ToLowerInvariant()
  if ($actualHash -ne $expectedHash) {
    Fail "SHA-256 mismatch for '$Asset': expected $expectedHash, got $actualHash (nothing was installed)"
  }
  Write-Host "install.ps1: checksum OK ($actualHash)"

  # --------------------------------------------------------------- install
  $target = Join-Path $Prefix $TargetName
  if ((Test-Path -LiteralPath $target) -and -not $Force) {
    Fail "$target already exists; pass -Force to replace it, or choose another -Prefix"
  }

  New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
  $staging = Join-Path $Prefix ('.pi-rs.install.' + [guid]::NewGuid().ToString('N') + '.tmp')
  try {
    Copy-Item -LiteralPath $binTmp -Destination $staging -Force
    # Windows executability comes from the .exe extension, so there is no
    # chmod 0755 equivalent to apply; the final name keeps the extension.
    Move-Item -LiteralPath $staging -Destination $target -Force
  } catch {
    if (Test-Path -LiteralPath $staging) {
      Remove-Item -LiteralPath $staging -Force -ErrorAction SilentlyContinue
    }
    throw
  }

  # ------------------------------------------------------------- user PATH
  if ($AddToPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $alreadyPresent = $false
    if (-not [string]::IsNullOrEmpty($userPath)) {
      foreach ($entry in ($userPath -split ';')) {
        if ($entry.Trim().TrimEnd('\') -ieq $Prefix.TrimEnd('\')) {
          $alreadyPresent = $true
          break
        }
      }
    }
    if ($alreadyPresent) {
      Write-Host "install.ps1: $Prefix is already on your user PATH"
    } else {
      if ([string]::IsNullOrEmpty($userPath)) {
        $newPath = $Prefix
      } else {
        $newPath = $userPath.TrimEnd(';') + ';' + $Prefix
      }
      [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
      Write-Host "install.ps1: added $Prefix to your user PATH (restart your shell to pick it up)"
    }
  }

  # ------------------------------------------------------------ next steps
  Write-Host ''
  Write-Host "pi-rs $Version installed successfully: $target"
  Write-Host ''
  Write-Host 'Next steps:'
  Write-Host "  1. Check the binary:  & '$target' --help"
  Write-Host "  2. Try a prompt:      & '$target' -p 'Say hello'"
  if (-not $AddToPath) {
    Write-Host "  3. To use 'pi-rs' from any shell, add $Prefix to your PATH, or re-run with -AddToPath."
  }
  Write-Host '  4. Sessions/config live under pi-rs per-user data roots; see README.md -> Paths.'
  Write-Host ''
  Write-Host 'Uninstall:'
  Write-Host "  Remove-Item -LiteralPath '$target'"
  Write-Host "  # If you used -AddToPath, also remove '$Prefix' from your user PATH."
} finally {
  if (Test-Path -LiteralPath $work) {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
  }
}
