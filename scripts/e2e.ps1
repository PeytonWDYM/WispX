param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/e2e"
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
Add-Type -AssemblyName System.Speech
$speech = New-Object System.Speech.Synthesis.SpeechSynthesizer
$format = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$wav = Join-Path $OutDir 'input.wav'
try {
    $speech.SetOutputToWaveFile($wav, $format)
    $speech.Speak('This is a repeatable WispX dictation test. The quick brown fox jumps over the lazy dog.')
} finally { $speech.Dispose() }

# These entry points bypass installation, named-instance events, tray and hooks.
$checks = @(
    @{ Args = @('--portable', '--selftest', $wav, (Join-Path $OutDir 'selftest.log')) },
    @{ Args = @('--portable', '--options-tour', (Join-Path $OutDir 'options')) },
    @{ Args = @('--export-sounds', (Join-Path $OutDir 'sounds')) }
)
$failed = $false
foreach ($check in $checks) {
    # Start-Process quotes arguments explicitly because paths may contain spaces.
    $quoted = $check.Args | ForEach-Object { '"' + $_ + '"' }
    $process = Start-Process -FilePath $Exe -ArgumentList $quoted -WindowStyle Hidden -PassThru -Wait
    if ($process.ExitCode -ne 0) { $failed = $true; Write-Warning "Failed ($($process.ExitCode)): $($check.Args -join ' ')" }
}
Get-ChildItem -LiteralPath $OutDir -Recurse -File | Select-Object FullName, Length
if ($failed) { throw 'An E2E entry point failed. See selftest.log and the generated artifacts.' }
