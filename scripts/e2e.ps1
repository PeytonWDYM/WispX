param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/e2e"
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
& "$PSScriptRoot/context-e2e.ps1" -Exe $Exe -OutDir (Join-Path $OutDir 'context')
& "$PSScriptRoot/stats-e2e.ps1" -Exe $Exe -OutDir (Join-Path $OutDir 'stats')
Add-Type -AssemblyName System.Speech
Get-Command ffprobe, ffmpeg -ErrorAction Stop | Out-Null
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

# Inspect the exact bytes the selftest uploaded using an independent demuxer/decoder.
$webm = Join-Path $OutDir 'selftest.webm'
$probeOutput = & ffprobe -v error -show_streams -show_format -of json $webm
$probeOutput | Set-Content -LiteralPath (Join-Path $OutDir 'ffprobe.json') -Encoding UTF8
if ($LASTEXITCODE -ne 0) { throw 'ffprobe rejected the WebM upload. See ffprobe.json.' }
$probe = $probeOutput | ConvertFrom-Json
if ($probe.streams.Count -ne 1 -or $probe.streams[0].codec_name -ne 'opus' -or $probe.streams[0].sample_rate -ne '48000' -or $probe.streams[0].channels -ne 1) {
    throw 'Expected one mono 48 kHz Opus stream in the WebM upload.'
}
# WebM already delimits packets. FFmpeg 8.0.1's redundant Opus parser tries to
# parse the empty EOF packet (also on libopus output); skip it, keeping decoding strict.
$decodeArgs = @('-v', 'error', '-fflags', '+noparse+nofillin', '-xerror', '-y', '-i', ('"' + $webm + '"'), '-acodec', 'pcm_s16le', ('"' + (Join-Path $OutDir 'decoded.wav') + '"'))
$decode = Start-Process -FilePath 'ffmpeg' -ArgumentList $decodeArgs -WindowStyle Hidden -Wait -PassThru -RedirectStandardError (Join-Path $OutDir 'ffmpeg.log')
if ($decode.ExitCode -ne 0) { throw 'ffmpeg could not decode the WebM upload cleanly. See ffmpeg.log.' }
$inputProbe = & ffprobe -v error -show_entries stream=sample_rate,duration_ts -of json $wav
if ($LASTEXITCODE -ne 0) { throw 'ffprobe could not inspect the input WAV.' }
$decodedProbe = & ffprobe -v error -show_entries stream=sample_rate,duration_ts -of json (Join-Path $OutDir 'decoded.wav')
$decodedProbe | Set-Content -LiteralPath (Join-Path $OutDir 'decoded-ffprobe.json') -Encoding UTF8
if ($LASTEXITCODE -ne 0) { throw 'ffprobe could not inspect the decoded WAV.' }
$inputStream = ($inputProbe | ConvertFrom-Json).streams[0]
$decodedStream = ($decodedProbe | ConvertFrom-Json).streams[0]
if ([long]$decodedStream.duration_ts * [long]$inputStream.sample_rate -ne [long]$inputStream.duration_ts * [long]$decodedStream.sample_rate) {
    throw 'Decoded duration differs from the input WAV: check pre-skip and end padding.'
}

# Raw transcription spells "WispX" differently run to run (Wispex, Wispecs...). The cleanup
# pass, with "WispX" in its dictionary, must return the fixture sentence exactly.
$cleanupLine = Get-Content -LiteralPath (Join-Path $OutDir 'selftest.log') | Where-Object { $_ -match '^\S+ \S+ \S+ CLEANUP ' } | Select-Object -Last 1
if (-not $cleanupLine) { throw 'No cleaned transcript was recorded. See selftest.log.' }
$cleaned = [regex]::Match($cleanupLine, 'cleaned="(.*)"$').Groups[1].Value
$expected = 'This is a repeatable WispX dictation test. The quick brown fox jumps over the lazy dog.'
if ($cleaned -cne $expected) { throw "Cleaned transcript differs from the fixture: $cleaned" }
Get-ChildItem -LiteralPath $OutDir -Recurse -File | Select-Object FullName, Length
if ($failed) { throw 'An E2E entry point failed. See selftest.log and the generated artifacts.' }
