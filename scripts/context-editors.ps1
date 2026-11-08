param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/editors"
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$temp = Join-Path ([IO.Path]::GetTempPath()) ('wispx-editors-' + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temp | Out-Null
$file = Join-Path $temp 'wispx-context-fixture.txt'
[IO.File]::WriteAllText($file, 'Before the caret. After the caret.')
foreach ($editor in @('vscode', 'vscode-accessible', 'notepad')) {
    if ($editor -like 'vscode*') {
        $app = Join-Path $env:LOCALAPPDATA 'Programs/Microsoft VS Code/Code.exe'
        $profile = Join-Path $temp $editor
        $extensions = Join-Path $temp 'code-extensions'
        New-Item -ItemType Directory -Force -Path (Join-Path $profile 'User') | Out-Null
        $settings = @{ 'workbench.startupEditor' = 'none'; 'window.restoreWindows' = 'none'; 'update.mode' = 'none' }
        if ($editor -eq 'vscode-accessible') { $settings['editor.accessibilitySupport'] = 'on' }
        [IO.File]::WriteAllText((Join-Path $profile 'User/settings.json'), ($settings | ConvertTo-Json))
        # T3's command environment runs Electron as Node; remove that flag only
        # from the isolated editor child so it launches its normal UI.
        $launch = New-Object Diagnostics.ProcessStartInfo
        $launch.FileName = $app
        $launch.UseShellExecute = $false
        $launch.EnvironmentVariables.Remove('ELECTRON_RUN_AS_NODE')
        $launch.Arguments = @('--user-data-dir="' + $profile + '"', '--extensions-dir="' + $extensions + '"', '--disable-extensions', '--disable-features=CalculateNativeWinOcclusion', '--skip-welcome', '--skip-release-notes', '--new-window', '"' + $file + '"') -join ' '
        $process = [Diagnostics.Process]::Start($launch)
    } else {
        # Notepad can reuse existing tabs. Refuse to touch a daily-driver instance.
        if (Get-Process Notepad -ErrorAction SilentlyContinue) { throw 'Close existing Notepad instances before the isolated editor investigation.' }
        $process = Start-Process notepad.exe -ArgumentList ('"' + $file + '"') -PassThru
    }
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        do {
            Start-Sleep -Milliseconds 100
            if ($editor -like 'vscode*') {
                $opened = @(Get-CimInstance Win32_Process -Filter "name = 'Code.exe'" | Where-Object { $_.CommandLine -like "*$profile*" -and $_.CommandLine -notmatch '--type=' } | ForEach-Object { Get-Process -Id $_.ProcessId } | Where-Object MainWindowTitle -like '*wispx-context-fixture*')
                if ($opened.Count -eq 1) { $process = $opened[0] }
            }
            if ($editor -eq 'notepad') {
                $opened = @(Get-Process Notepad -ErrorAction SilentlyContinue | Where-Object MainWindowTitle -like '*wispx-context-fixture*')
                if ($opened.Count -eq 1) { $process = $opened[0] }
            }
            $process.Refresh()
        } while (($process.MainWindowHandle -eq 0 -or $process.MainWindowTitle -notlike '*wispx-context-fixture*') -and [DateTime]::UtcNow -lt $deadline)
        if ($process.MainWindowHandle -eq 0 -or $process.MainWindowTitle -notlike '*wispx-context-fixture*') { throw "No isolated $editor window" }
        $request = Join-Path $OutDir "$editor-request.json"
        $out = Join-Path $OutDir "$editor.json"
        $control = if ($editor -eq 'notepad') { 50030 } else { 50004 }
        $spec = @{ hwnd = $process.MainWindowHandle.ToInt64(); control_type = $control; out = $out }
        if ($editor -ne 'vscode') { $spec['caret'] = 18 }
        [IO.File]::WriteAllText($request, ($spec | ConvertTo-Json))
        $probe = Start-Process $Exe -ArgumentList @('--context-probe', ('"' + $request + '"')) -WindowStyle Hidden -Wait -PassThru -RedirectStandardError (Join-Path $OutDir "$editor.stderr")
        if ($probe.ExitCode -ne 0) { throw "Probe failed for $editor; see $editor.stderr" }
        Get-Content -LiteralPath $out | Write-Output
        if ($editor -ne 'vscode') {
            foreach ($sample in (Get-Content -Raw -LiteralPath $out | ConvertFrom-Json)) {
                if ($sample.text -cne 'Before the caret. [cursor]After the caret.') { throw "$editor context did not match the fixture caret" }
            }
        }
    } finally {
        $process.Refresh()
        if (-not $process.HasExited) { [void]$process.CloseMainWindow(); [void]$process.WaitForExit(10000) }
    }
}
