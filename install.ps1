<#
.SYNOPSIS
    Installs the airec screen recorder CLI on Windows.

.DESCRIPTION
    Downloads a prebuilt airec.exe from the GitHub releases of
    j-token/airec, verifies its SHA256, places it in a per-user
    directory, and puts that directory on the user PATH. No admin rights and no
    Rust toolchain are needed.

.EXAMPLE
    irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1))) -Version v0.2.2
#>
[CmdletBinding()]
param(
    # Release tag to install, or "latest".
    [string]$Version = $(if ($env:AIREC_VERSION) { $env:AIREC_VERSION } else { "latest" }),

    # Directory the executable is placed in.
    [string]$InstallDir = $(if ($env:AIREC_INSTALL_DIR) { $env:AIREC_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "airec\bin" }),

    # Remove a previous installation instead of installing.
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = "j-token/airec"
$Target = "x86_64-pc-windows-msvc"
$ExePath = Join-Path $InstallDir "airec.exe"

function Write-Step($message) { Write-Host "==> $message" }

# The user PATH is read and written through the registry rather than through
# [Environment]::SetEnvironmentVariable, which rewrites the value as REG_SZ and
# would permanently expand any %VAR% entries the user already has there.
function Get-UserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $false)
    try {
        return [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        if ($key) { $key.Close() }
    }
}

function Set-UserPath($value) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    try {
        # Keep whatever type the value already had; only a missing Path is created
        # as ExpandString, which is what Windows itself uses.
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        if ($key.GetValueNames() -contains "Path") { $kind = $key.GetValueKind("Path") }
        $key.SetValue("Path", $value, $kind)
    } finally {
        if ($key) { $key.Close() }
    }
    Send-SettingChange
}

# Without this, processes started from Explorer keep the stale environment until
# the user signs out.
function Send-SettingChange {
    if (-not ("AirecEnv" -as [type])) {
        Add-Type -Namespace "" -Name AirecEnv -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
    }
    $result = [UIntPtr]::Zero
    [void][AirecEnv]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$result)
}

function Test-SamePath($a, $b) {
    return $a.TrimEnd('\').TrimEnd('/') -eq $b.TrimEnd('\').TrimEnd('/')
}

if ($Uninstall) {
    if (Test-Path $ExePath) {
        Remove-Item $ExePath -Force
        Write-Step "removed $ExePath"
    }
    if ((Test-Path $InstallDir) -and -not (Get-ChildItem $InstallDir -Force)) {
        Remove-Item $InstallDir -Force
    }
    $current = Get-UserPath
    $kept = @($current -split ';' | Where-Object { $_ -and -not (Test-SamePath $_ $InstallDir) })
    if ($kept.Count -ne @($current -split ';' | Where-Object { $_ }).Count) {
        Set-UserPath ($kept -join ';')
        Write-Step "removed $InstallDir from the user PATH"
    }
    Write-Host "airec uninstalled. Recordings and airec.toml were left untouched."
    return
}

# airec needs Windows Graphics Capture, which shipped in Windows 10 2004 (build 19041).
$build = [int](Get-CimInstance Win32_OperatingSystem).BuildNumber
if ($build -lt 19041) {
    Write-Warning "Windows build $build is older than 19041 (Windows 10 2004). airec will install but capture is unlikely to work."
}

if ($env:PROCESSOR_ARCHITECTURE -notin @("AMD64", "ARM64")) {
    throw "airec ships only an x86_64 build; this machine reports $env:PROCESSOR_ARCHITECTURE."
}

if (Get-Process -Name airec -ErrorAction SilentlyContinue) {
    throw "airec is currently running. Run 'airec stop' or close it, then install again."
}

Write-Step "resolving $Version"
$headers = @{ "User-Agent" = "airec-install" }
if ($env:GITHUB_TOKEN) { $headers["Authorization"] = "Bearer $env:GITHUB_TOKEN" }

$releaseUrl = if ($Version -eq "latest") {
    "https://api.github.com/repos/$Repo/releases/latest"
} else {
    "https://api.github.com/repos/$Repo/releases/tags/$Version"
}

try {
    $release = Invoke-RestMethod -Uri $releaseUrl -Headers $headers
} catch {
    throw "could not read release '$Version' from $Repo. Check the tag name, or see https://github.com/$Repo/releases. ($_)"
}

$tag = $release.tag_name
$assetName = "airec-$tag-$Target.zip"
$asset = $release.assets | Where-Object { $_.name -eq $assetName }
if (-not $asset) {
    $available = ($release.assets | ForEach-Object { $_.name }) -join ", "
    throw "release $tag has no asset named $assetName. Available: $available"
}
$checksumAsset = $release.assets | Where-Object { $_.name -eq "$assetName.sha256" }

$work = Join-Path ([IO.Path]::GetTempPath()) ("airec-install-" + [Guid]::NewGuid().ToString("n").Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $zip = Join-Path $work $assetName
    Write-Step "downloading $tag"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -Headers $headers

    if ($checksumAsset) {
        Write-Step "verifying checksum"
        $expected = ((Invoke-WebRequest -Uri $checksumAsset.browser_download_url -Headers $headers).Content -split '\s+')[0].Trim().ToLower()
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            throw "checksum mismatch for $assetName (expected $expected, got $actual). The download was not installed."
        }
    } else {
        Write-Warning "release $tag publishes no .sha256 file; skipping checksum verification."
    }

    Expand-Archive -Path $zip -DestinationPath $work -Force
    $extracted = Get-ChildItem $work -Recurse -Filter airec.exe | Select-Object -First 1
    if (-not $extracted) { throw "the archive did not contain airec.exe" }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item $extracted.FullName $ExePath -Force
    Write-Step "installed $ExePath"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = Get-UserPath
$onPath = @($userPath -split ';' | Where-Object { $_ -and (Test-SamePath $_ $InstallDir) }).Count -gt 0
if (-not $onPath) {
    $updated = if ($userPath) { "$($userPath.TrimEnd(';'));$InstallDir" } else { $InstallDir }
    Set-UserPath $updated
    Write-Step "added $InstallDir to the user PATH"
}
$env:Path = "$env:Path;$InstallDir"

& $ExePath --version
Write-Host ""
Write-Host "Open a new terminal so PATH takes effect, then run 'airec doctor --json' to confirm capture and encoding work on this machine."
