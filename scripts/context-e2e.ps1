param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/context",
    [switch]$Observe
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$profile = Join-Path ([IO.Path]::GetTempPath()) ('wispx-edge-' + [guid]::NewGuid())
$edge = Join-Path ${env:ProgramFiles(x86)} 'Microsoft/Edge/Application/msedge.exe'
$fixture = (Resolve-Path -LiteralPath "$PSScriptRoot/fixtures/context.html").Path
# Keep an occluded test renderer available to UIA while other tools are active.
# Accessibility itself stays lazy: do not use --force-renderer-accessibility.
$process = Start-Process -FilePath $edge -ArgumentList @(('--user-data-dir="' + $profile + '"'), '--no-first-run', '--no-default-browser-check', '--inprivate', '--disable-features=CalculateNativeWinOcclusion', '--new-window', ('"file:///' + $fixture.Replace('\','/') + '"')) -PassThru
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
    } while (($process.MainWindowHandle -eq 0 -or $process.MainWindowTitle -notlike '*WispX isolated context fixture*') -and [DateTime]::UtcNow -lt $deadline)
    if ($process.MainWindowHandle -eq 0) { throw 'Isolated Edge window did not open' }
    foreach ($field in @('cold', 'textarea', 'input', 'contenteditable', 'password')) {
        $out = Join-Path $OutDir "edge-$field.json"
        $request = Join-Path $OutDir 'request.json'
        $name = if ($field -eq 'cold') { $null } else { "WispX $field" }
        $json = @{ hwnd = $process.MainWindowHandle.ToInt64(); name = $name; out = $out } | ConvertTo-Json
        [IO.File]::WriteAllText($request, $json)
        $probe = Start-Process -FilePath $Exe -ArgumentList @('--context-probe', ('"' + $request + '"')) -WindowStyle Hidden -PassThru -Wait -RedirectStandardError (Join-Path $OutDir "edge-$field.stderr")
        if ($probe.ExitCode -ne 0) { throw "Context probe failed for $field" }
        $samples = Get-Content -Raw -LiteralPath $out | ConvertFrom-Json
        Write-Output "Edge $field : $($samples.Count) samples, capture ms: $($samples.capture_ms -join ', ')"
        if (-not $Observe) {
            foreach ($sample in $samples) {
                $expected = if ($field -eq 'password') { $null } else { 'Before the caret. [cursor]After the caret.' }
                if ($sample.text -cne $expected) { throw "Edge $field context mismatch: $($sample.text)" }
            }
        }
    }
} finally {
    # Only the window launched with our unique profile is closed; never terminate a process.
    $process.Refresh()
    if (-not $process.HasExited) {
        [void]$process.CloseMainWindow()
        if (-not $process.WaitForExit(10000)) { throw "Owned Edge instance did not close: $($process.Id)" }
    }
}
