# Codeyard seed-CD contract, not ported from actions/runner.
$ErrorActionPreference = 'Stop'
Start-Transcript -Path C:\ProgramData\Codeyard\job-key.log -Force
try {
    Stop-Service sshd -ErrorAction SilentlyContinue
    $authorized = 'C:\ProgramData\ssh\runner_authorized_keys'
    Remove-Item $authorized -Force -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddMinutes(5)
    $seed = $null
    do {
        $seed = Get-Volume | Where-Object { $_.DriveType -eq 'CD-ROM' -and $_.FileSystemLabel -eq 'CODEYARD' } | Select-Object -First 1
        if (!$seed) { Start-Sleep -Seconds 1 }
    } while (!$seed -and (Get-Date) -lt $deadline)
    if (!$seed) { throw 'No CODEYARD seed CD: SSH will remain stopped' }
    $source = $seed.DriveLetter + ':\authorized_keys'
    $key = (Get-Content -Raw -LiteralPath $source).Trim()
    if ($key -notmatch '^ssh-ed25519 [A-Za-z0-9+/]+={0,2}( [^\r\n]*)?$') { throw 'Invalid job SSH key' }
    [IO.File]::WriteAllText($authorized, $key + "`n", [Text.Encoding]::ASCII)
    & icacls.exe $authorized /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Failed to protect job key' }
    & icacls.exe $authorized /setowner '*S-1-5-18' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Failed to set job key owner' }
    # Every job gets fresh host keys as well as a fresh client key.
    Remove-Item C:\ProgramData\ssh\ssh_host_* -Force -ErrorAction SilentlyContinue
    & C:\Windows\System32\OpenSSH\ssh-keygen.exe -A
    if ($LASTEXITCODE -ne 0) { throw 'Failed to generate SSH host keys' }
    & C:\Windows\System32\OpenSSH\sshd.exe -t
    if ($LASTEXITCODE -ne 0) { throw 'Invalid sshd configuration' }
    Start-Service sshd
} catch {
    Stop-Service sshd -ErrorAction SilentlyContinue
    Write-Error -ErrorAction Continue $_
    exit 1
} finally {
    Stop-Transcript
}
