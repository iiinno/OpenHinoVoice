#Requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDir
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'X64') {
    throw 'Mesa preparation is restricted to GitHub-hosted Windows x64 runners.'
}
if (-not $env:GITHUB_WORKSPACE -or -not $env:RUNNER_TEMP -or -not $env:GITHUB_ENV) {
    throw 'Required GitHub Actions paths are missing.'
}
$OutputDir = [IO.Path]::GetFullPath($OutputDir).TrimEnd([IO.Path]::DirectorySeparatorChar)
$allowedRoots = @((Join-Path $env:GITHUB_WORKSPACE 'target/qa'), $env:RUNNER_TEMP)
$allowed = $false
foreach ($root in $allowedRoots) {
    $prefix = [IO.Path]::GetFullPath($root).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ($OutputDir.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { $allowed = $true }
}
if (-not $allowed) { throw 'OutputDir must be a dedicated child directory under target/qa or RUNNER_TEMP.' }
if ($OutputDir.Contains([char]10) -or $OutputDir.Contains([char]13)) { throw 'OutputDir must not contain line breaks.' }
if (Test-Path -LiteralPath $OutputDir) {
    if (-not (Test-Path -LiteralPath $OutputDir -PathType Container) -or @(Get-ChildItem -LiteralPath $OutputDir -Force).Count -ne 0) {
        throw 'OutputDir must be new or empty; existing binaries are never overwritten.'
    }
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$version = '26.2.6'
$archiveName = 'mesa-llvmpipe-x64-26.2.6.7z'
$sourceUrl = 'https://github.com/mmozeiko/build-mesa/releases/download/26.2.6/mesa-llvmpipe-x64-26.2.6.7z'
$expectedSha256 = '1910245962e3d854589ed11a9a488e84841a01559142fba8b81395271981700b'
$archivePath = Join-Path $OutputDir $archiveName
$dllPath = Join-Path $OutputDir 'opengl32.dll'
$resultPath = Join-Path $OutputDir 'mesa-preparation.json'
$result = [ordered]@{
    schemaVersion = 1; status = 'running'; version = $version; architecture = 'x64'
    startedAt = [DateTime]::UtcNow.ToString('o'); finishedAt = $null
    sourceRevision = $env:GITHUB_SHA; sourceUrl = $sourceUrl
    digestSource = 'https://github.com/mmozeiko/build-mesa/releases/expanded_assets/26.2.6'
    usageSource = 'https://github.com/mmozeiko/build-mesa/blob/26.2.6/README.md'
    expectedArchiveSha256 = $expectedSha256; archiveSha256 = $null
    dllPath = $dllPath; dllSha256 = $null; extraction = 'not_run'; environmentExport = 'not_run'
    runtimeValidation = 'not_run'; productFilesChanged = $false
    limitations = @('The distributor documents DLL deployment next to the test executable. The caller must place a verified isolated executable copy in OutputDir before testing.', 'An absolute GLUTIN_WGL_OPENGL_DLL override alone does not prove that every WGL import uses Mesa.', 'Software-rendered results do not measure end-user GPU performance.')
}
function Save-Preparation {
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $resultPath -Encoding utf8NoBOM
}
$exitCode = 1
try {
    Save-Preparation
    Invoke-WebRequest -Uri $sourceUrl -OutFile $archivePath -TimeoutSec 120 -MaximumRetryCount 2
    $result.archiveSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($result.archiveSha256 -cne $expectedSha256) { throw 'The pinned Mesa archive SHA256 did not match; extraction was not attempted.' }
    Save-Preparation
    $sevenZip = Join-Path $env:ProgramFiles '7-Zip/7z.exe'
    if (-not (Test-Path -LiteralPath $sevenZip -PathType Leaf)) {
        $sevenZip = (Get-Command '7z.exe' -CommandType Application -ErrorAction Stop).Source
    }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $sevenZip; $info.WorkingDirectory = $OutputDir
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    # Extract only the WGL DLL; no installer, shell script, or release binary is executed.
    foreach ($argument in @('x', '-y', "-o$OutputDir", '--', $archivePath, 'opengl32.dll')) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $started = $false; $stdout = $null; $stderr = $null
    $result.extraction = 'running'; Save-Preparation
    try {
        $started = $process.Start()
        if (-not $started) { throw 'Could not start the hosted runner 7-Zip utility.' }
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(60000)) { throw 'Mesa archive extraction exceeded its deadline.' }
        if ($process.ExitCode -ne 0) { throw "Mesa extraction failed with code $($process.ExitCode)." }
    } finally {
        try {
            if ($started -and -not $process.HasExited) {
                $process.Kill($true)
                if (-not $process.WaitForExit(5000)) { throw 'The extraction process did not stop after termination.' }
            }
            if ($null -ne $stdout) {
                [IO.File]::WriteAllText((Join-Path $OutputDir 'extraction.log'), $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult())
            }
        } finally { $process.Dispose() }
    }
    if (-not (Test-Path -LiteralPath $dllPath -PathType Leaf) -or (Get-Item -LiteralPath $dllPath).Length -eq 0) {
        throw 'The verified archive did not produce opengl32.dll at OutputDir.'
    }
    $result.dllSha256 = (Get-FileHash -LiteralPath $dllPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $result.extraction = 'passed'
    @("GLUTIN_WGL_OPENGL_DLL=$dllPath", 'GALLIUM_DRIVER=llvmpipe') | Add-Content -LiteralPath $env:GITHUB_ENV -Encoding utf8NoBOM
    $result.environmentExport = 'passed'; $result.status = 'prepared'; $exitCode = 0
} catch {
    if ($result.extraction -eq 'running') { $result.extraction = 'failed' }
    $result.status = 'failed'; $result.error = $_.Exception.Message
} finally {
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    Save-Preparation
    Write-Output $resultPath
}
exit $exitCode
