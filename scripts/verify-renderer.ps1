#Requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Executable,
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [ValidateRange(1000, 300000)][int]$TimeoutMs = 120000
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'Renderer animation verification is restricted to GitHub-hosted Windows runners.'
}
$repo = Split-Path -Parent $PSScriptRoot
$Executable = [IO.Path]::GetFullPath($Executable)
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$resultPath = Join-Path $OutputDir 'result.json'
$result = [ordered]@{
    schemaVersion = 1; mode = 'AnimatedOffscreenRenderer'; status = 'running'
    startedAt = [DateTime]::UtcNow.ToString('o'); finishedAt = $null
    sourceRevision = $env:GITHUB_SHA; executable = $Executable; executableSha256 = $null
    scope = 'shared preview_frame rasterization and RGBA conversion with system motion enabled'
    fixtures = 'not_run'; rendererBenchmark = 'not_run'; frameCount = 0; measurementCount = 0
    motion = [ordered]@{ before = $null; afterEnable = $null; afterRestore = $null; restore = 'not_run'; profileUpdated = $false }
    wholeApplicationPerformanceBenchmark = 'not_run'; audioRequested = $false; microphoneRequested = $false
    limitations = @('Excludes settings texture upload, native window compositing, scheduling, input latency, and ASR.', 'Hosted runner timings are not end-user machine performance.')
}
function Save-Result {
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $resultPath -Encoding utf8NoBOM
}
function Invoke-Renderer([string[]]$Arguments, [string]$LogPath) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable; $info.WorkingDirectory = $repo
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $started = $false; $stdout = $null; $stderr = $null; $failure = $null
    try {
        $started = $process.Start()
        if (-not $started) { throw 'Could not start the prebuilt renderer.' }
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutMs)) { throw 'Renderer execution exceeded its deadline.' }
        if ($process.ExitCode -ne 0) { throw "Renderer exited with code $($process.ExitCode); inspect $LogPath." }
    } catch {
        $failure = $_.Exception.Message
        throw
    } finally {
        try {
            if ($started -and -not $process.HasExited) {
                $process.Kill($true)
                if (-not $process.WaitForExit(5000)) { throw 'The renderer process did not stop after termination.' }
            }
            $captured = if ($null -ne $stdout) { $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult() } else { '' }
            if ($failure) { $captured += [Environment]::NewLine + $failure }
            [IO.File]::WriteAllText($LogPath, $captured)
        } finally {
            $process.Dispose()
        }
    }
}
$restoreRequired = $false
$exitCode = 1
try {
    Save-Result
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw 'The prebuilt renderer executable is missing.' }
    $result.executableSha256 = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
    # GET takes BOOL*, while SET takes the TRUE/FALSE value cast to PVOID.
    # https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfow
    if (-not ('OpenHinoVoice.RendererCi.Motion' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
namespace OpenHinoVoice.RendererCi {
    public static class Motion {
        [DllImport("user32.dll", EntryPoint = "SystemParametersInfoW", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool Query(uint action, uint parameter, [MarshalAs(UnmanagedType.Bool)] out bool value, uint flags);
        [DllImport("user32.dll", EntryPoint = "SystemParametersInfoW", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool Set(uint action, uint parameter, IntPtr value, uint flags);
        public static bool Read() {
            bool enabled;
            if (!Query(0x1042, 0, out enabled, 0)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SPI_GETCLIENTAREAANIMATION failed");
            }
            return enabled;
        }
        public static void Write(bool enabled) {
            if (!Set(0x1043, 0, new IntPtr(enabled ? 1 : 0), 0)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SPI_SETCLIENTAREAANIMATION failed");
            }
        }
    }
}
'@
    }
    $result.motion.before = [OpenHinoVoice.RendererCi.Motion]::Read()
    $restoreRequired = $true
    $result.motion.restore = 'pending'
    [OpenHinoVoice.RendererCi.Motion]::Write($true)
    $result.motion.afterEnable = [OpenHinoVoice.RendererCi.Motion]::Read()
    Save-Result
    if ($result.motion.afterEnable -ne $true) { throw 'Windows did not enable client-area animations.' }

    $fixtureDir = Join-Path $OutputDir 'overlay'
    $result.fixtures = 'running'; Save-Result
    Invoke-Renderer @($fixtureDir, '--all-dpi') (Join-Path $OutputDir 'render.log')
    $index = Get-Content -LiteralPath (Join-Path $fixtureDir 'index.json') -Raw | ConvertFrom-Json
    $result.frameCount = @($index.frames).Count
    if ($result.frameCount -ne 720 -or @($index.frames.file | Sort-Object -Unique).Count -ne 720) {
        throw 'Expected 720 distinct style/theme/state/motion/DPI fixtures.'
    }
    if ($index.system_motion_enabled -ne $true) { throw 'The fixture renderer did not observe enabled system motion.' }
    if ($index.windows_created -ne 0 -or $index.audio_played -ne $false -or $index.microphone_accessed -ne $false) {
        throw 'The fixture renderer reported unexpected side effects.'
    }
    foreach ($frame in $index.frames) {
        $path = Join-Path $fixtureDir $frame.file
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -le 54) {
            throw "Missing or empty fixture: $path"
        }
    }
    $result.fixtures = 'passed'

    $benchmarkDir = Join-Path $OutputDir 'renderer-benchmark'
    $result.rendererBenchmark = 'running'; Save-Result
    Invoke-Renderer @($benchmarkDir, '--benchmark') (Join-Path $OutputDir 'renderer-benchmark.log')
    $benchmark = Get-Content -LiteralPath (Join-Path $benchmarkDir 'benchmark.json') -Raw | ConvertFrom-Json
    $result.measurementCount = @($benchmark.measurements).Count
    if ($benchmark.status -ne 'measured' -or $result.measurementCount -ne 24) { throw 'Expected all 24 renderer measurements within the existing resource bounds.' }
    if ($benchmark.conditions.system_motion_enabled -ne $true) { throw 'The benchmark did not observe enabled system motion.' }
    if ($benchmark.conditions.audio -ne $false -or $benchmark.conditions.windows -ne $false) { throw 'The benchmark reported unexpected side effects.' }
    $result.rendererBenchmark = 'passed'; $result.status = 'passed'; $exitCode = 0
} catch {
    if ($result.fixtures -eq 'running') { $result.fixtures = 'failed' }
    if ($result.rendererBenchmark -eq 'running') { $result.rendererBenchmark = 'failed' }
    $result.status = 'failed'; $result.error = $_.Exception.Message
} finally {
    if ($restoreRequired) {
        try {
            [OpenHinoVoice.RendererCi.Motion]::Write([bool]$result.motion.before)
            $result.motion.afterRestore = [OpenHinoVoice.RendererCi.Motion]::Read()
            if ($result.motion.afterRestore -ne $result.motion.before) { throw 'Windows client-area animation setting was not restored.' }
            $result.motion.restore = 'passed'
        } catch {
            $result.motion.restore = 'failed'; $result.motion.restoreError = $_.Exception.Message
            $result.status = 'failed'; $exitCode = 1
        }
    }
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    Save-Result
    Write-Output $resultPath
}
exit $exitCode
