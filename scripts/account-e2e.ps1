param(
    [string]$Exe = "$PSScriptRoot/../target/debug/WispX.exe",
    [string]$OutDir = "$PSScriptRoot/../artifacts/e2e/account"
)
# Shoots the Account page with a fixture Codex login: the email blurred, then revealed.
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('wispx-account-e2e-' + [guid]::NewGuid())
$previous = @{ APPDATA = $env:APPDATA; USERPROFILE = $env:USERPROFILE; WISPX_TOUR_THEME = $env:WISPX_TOUR_THEME }
function ConvertTo-Base64Url([string]$json) {
    [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}
try {
    New-Item -ItemType Directory -Path (Join-Path $fixture 'WispX'), (Join-Path $fixture '.codex') -Force | Out-Null
    # Only the id_token's email claim and a non-empty access token matter to the Account page.
    $idToken = (ConvertTo-Base64Url '{"alg":"none"}') + '.' + (ConvertTo-Base64Url '{"email":"ada.lovelace@example.com"}') + '.sig'
    @{ auth_mode = 'chatgpt'; tokens = @{ id_token = $idToken; access_token = 'fixture'; account_id = 'fixture' } } |
        ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $fixture '.codex/auth.json') -Encoding ASCII
    $env:APPDATA = $fixture
    $env:USERPROFILE = $fixture
    foreach ($theme in @('dark', 'light')) {
        $env:WISPX_TOUR_THEME = $theme
        $destination = Join-Path $OutDir $theme
        New-Item -ItemType Directory -Force -Path $destination | Out-Null
        $process = Start-Process -FilePath $Exe -ArgumentList @('--portable', '--options-tour', ('"' + $destination + '"')) -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne 0) { throw "Account tour failed ($theme): $($process.ExitCode)" }
        $blurred = Get-Item -LiteralPath (Join-Path $destination '1-account.png')
        $revealed = Get-Item -LiteralPath (Join-Path $destination '1-account-revealed.png')
        if ((Get-FileHash $blurred).Hash -eq (Get-FileHash $revealed).Hash) {
            throw "Revealing the email changed nothing ($theme)"
        }
        Write-Output $blurred.FullName $revealed.FullName
    }
} finally {
    foreach ($name in $previous.Keys) { Set-Item -Path "env:$name" -Value $previous[$name] }
    # Only remove the exact fixture directory created above, under the system temp directory.
    $resolvedFixture = [IO.Path]::GetFullPath($fixture)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (-not $resolvedFixture.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a fixture outside the temp directory: $resolvedFixture"
    }
    Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
}
