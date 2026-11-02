# Codeyard image provisioning, not ported from actions/runner.
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force C:\ProgramData\Codeyard | Out-Null
Start-Transcript -Path C:\ProgramData\Codeyard\provision.log -Force

function Install-Msi([string] $Path, [string] $Properties = '') {
    $p = Start-Process msiexec.exe -ArgumentList "/i `"$Path`" /qn /norestart $Properties" -Wait -PassThru
    if ($p.ExitCode -notin @(0, 3010)) { throw "MSI failed: $Path ($($p.ExitCode))" }
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $downloads = 'C:\ProgramData\Codeyard\downloads'
    New-Item -ItemType Directory -Force $downloads | Out-Null

    # Official PowerShell release; Authenticode is checked before running installers.
    $pwsh = Join-Path $downloads 'powershell.msi'
    Invoke-WebRequest -UseBasicParsing 'https://github.com/PowerShell/PowerShell/releases/download/v7.4.13/PowerShell-7.4.13-win-x64.msi' -OutFile $pwsh
    if ((Get-AuthenticodeSignature $pwsh).Status -ne 'Valid') { throw 'Invalid PowerShell installer signature' }
    Install-Msi $pwsh 'ADD_PATH=1 ENABLE_PSREMOTING=0 REGISTER_MANIFEST=1'

    $git = Join-Path $downloads 'git.exe'
    Invoke-WebRequest -UseBasicParsing 'https://github.com/git-for-windows/git/releases/download/v2.49.0.windows.1/Git-2.49.0-64-bit.exe' -OutFile $git
    if ((Get-AuthenticodeSignature $git).Status -ne 'Valid') { throw 'Invalid Git installer signature' }
    $p = Start-Process $git -ArgumentList '/VERYSILENT /NORESTART /NOCANCEL /SP- /o:PathOption=Cmd' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "Git installer failed: $($p.ExitCode)" }

    $releases = Invoke-RestMethod 'https://nodejs.org/dist/index.json'
    $nodeVersion = ($releases | Where-Object { $_.lts -ne $false } | Select-Object -First 1).version
    if ($nodeVersion -notmatch '^v\d+\.\d+\.\d+$') { throw 'Invalid Node LTS version' }
    $node = Join-Path $downloads 'node.msi'
    Invoke-WebRequest -UseBasicParsing "https://nodejs.org/dist/$nodeVersion/node-$nodeVersion-x64.msi" -OutFile $node
    if ((Get-AuthenticodeSignature $node).Status -ne 'Valid') { throw 'Invalid Node installer signature' }
    Install-Msi $node

    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
    if (!(Test-Path C:\Windows\System32\OpenSSH\sshd.exe)) { throw 'OpenSSH server installation failed' }
    Stop-Service sshd -ErrorAction SilentlyContinue
    Set-Service sshd -StartupType Manual
    New-Item -ItemType Directory -Force C:\ProgramData\ssh | Out-Null
    @'
Port 22
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
PermitEmptyPasswords no
AllowUsers runner
DisableForwarding yes
AuthorizedKeysFile C:/ProgramData/ssh/runner_authorized_keys
Subsystem sftp C:/Windows/System32/OpenSSH/sftp-server.exe
'@ | Set-Content -Encoding ascii C:\ProgramData\ssh\sshd_config
    New-Item -Path HKLM:\SOFTWARE\OpenSSH -Force | Out-Null
    # The SSH command explicitly invokes pwsh. An outer PowerShell -Command shell
    # would collapse native exit codes (for example 7) to 1.
    New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -Value 'C:\Windows\System32\cmd.exe' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShellCommandOption -Value '/c' -PropertyType String -Force | Out-Null
    if (!(Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
    }
    Enable-NetFirewallRule -Name 'OpenSSH-Server-In-TCP'

    $random = New-Object byte[] 48
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($random)
    $rng.Dispose()
    $password = ConvertTo-SecureString ([Convert]::ToBase64String($random) + 'aA1!') -AsPlainText -Force
    New-LocalUser -Name runner -Password $password -PasswordNeverExpires -AccountNeverExpires | Out-Null
    $admins = Get-LocalGroup -SID 'S-1-5-32-544'
    Add-LocalGroupMember -Group $admins -Member runner
    New-Item -ItemType Directory -Force C:\codeyard\workspace, C:\codeyard\tmp | Out-Null
    # Administrator jobs match hosted Windows runners, but remain untrusted guest code.
    & icacls.exe C:\codeyard /grant '*S-1-5-32-544:(OI)(CI)F' /T | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Failed to set workspace permissions' }

    Copy-Item (Join-Path $PSScriptRoot 'Import-JobKey.ps1') C:\ProgramData\Codeyard\Import-JobKey.ps1
    $action = New-ScheduledTaskAction -Execute powershell.exe -Argument '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\ProgramData\Codeyard\Import-JobKey.ps1'
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -StartWhenAvailable
    Register-ScheduledTask -TaskName CodeyardJobKey -Action $action -Trigger $trigger -Settings $settings -User SYSTEM -RunLevel Highest -Force | Out-Null

    # Do not leave the generated installation password or an automatic admin login in the base.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Remove-ItemProperty $winlogon -Name DefaultPassword, DefaultUserName, AutoLogonCount -ErrorAction SilentlyContinue
    Set-ItemProperty $winlogon -Name AutoAdminLogon -Value '0'
    Get-ChildItem C:\Windows\Panther -Filter '*unattend*.xml' -Recurse -ErrorAction SilentlyContinue | Remove-Item -Force
    Remove-Item C:\Windows\System32\Sysprep\unattend.xml -Force -ErrorAction SilentlyContinue
    Disable-LocalUser -Name Administrator
    Remove-Item "$downloads\*" -Force
    Remove-Item C:\ProgramData\ssh\*key* -Force -ErrorAction SilentlyContinue
    Stop-Transcript
    $serial = New-Object System.IO.Ports.SerialPort COM1,115200,None,8,one
    $serial.Open()
    $serial.WriteLine('CODEYARD_IMAGE_READY')
    $serial.Close()
    Stop-Computer -Force
} catch {
    Write-Error -ErrorAction Continue $_
    Stop-Transcript
    # Deliberately stay running for console diagnosis; the host build times out.
    exit 1
}
