param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/e2e/stats"
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('wispx-stats-e2e-' + [guid]::NewGuid())
$config = Join-Path $fixture 'WispX'
$previousAppData = $env:APPDATA
$previousTheme = $env:WISPX_TOUR_THEME
try {
    New-Item -ItemType Directory -Path $config -Force | Out-Null
    $days = [ordered]@{}
    # A weekday workload leaves realistic weekend gaps in the 30-day series.
    $words = @(220, 410, 330, 580, 760, 480, 640, 910, 720, 1050, 880, 620, 1190, 940, 1320, 1010, 780, 1240, 1470, 1120)
    $dates = @(29..0 | ForEach-Object { (Get-Date).Date.AddDays(-$_) } | Where-Object { $_.DayOfWeek -notin @('Saturday', 'Sunday') } | Select-Object -Last 20)
    for ($i = 0; $i -lt $words.Count; $i++) {
        $days[$dates[$i].ToString('yyyy-MM-dd')] = @{
            Words = $words[$i]
            Dictations = [int][math]::Ceiling($words[$i] / 75)
            SpokenMs = [long]($words[$i] * 60000 / 145)
        }
    }
    @{ Days = $days } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $config 'stats.json') -Encoding ASCII
    $env:APPDATA = $fixture
    foreach ($state in @('populated', 'empty')) {
        foreach ($theme in @('dark', 'light')) {
            $env:WISPX_TOUR_THEME = $theme
            $destination = Join-Path (Join-Path $OutDir $state) $theme
            New-Item -ItemType Directory -Force -Path $destination | Out-Null
            $process = Start-Process -FilePath $Exe -ArgumentList @('--portable', '--options-tour', ('"' + $destination + '"')) -WindowStyle Hidden -Wait -PassThru
            if ($process.ExitCode -ne 0) { throw "Stats tour failed ($state, $theme): $($process.ExitCode)" }
            $screenshot = Get-Item -LiteralPath (Join-Path $destination '5-stats.png')
            if ($screenshot.Length -lt 10000) { throw "Stats screenshot is unexpectedly small: $($screenshot.FullName)" }
            Write-Output $screenshot.FullName
        }
        # Keep both runs isolated from the user's real settings and statistics.
        if ($state -eq 'populated') {
            Remove-Item -LiteralPath (Join-Path $config 'stats.json')
        }
    }
} finally {
    $env:APPDATA = $previousAppData
    $env:WISPX_TOUR_THEME = $previousTheme
    # Only remove the exact fixture directory created above, under the system temp directory.
    $resolvedFixture = [IO.Path]::GetFullPath($fixture)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (-not $resolvedFixture.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a fixture outside the temp directory: $resolvedFixture"
    }
    Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
}
