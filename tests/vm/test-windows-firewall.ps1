param(
    [string] $Source = (Join-Path $PSScriptRoot '../../backend/runner/scripts/windows-base-image/Import-JobKey.ps1')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Source, [ref] $tokens, [ref] $parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
$definition = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Set-JobSshFirewall'
}, $true)
if (!$definition) { throw 'Missing firewall policy function' }
# Load only the policy function: never execute the seed importer or alter real firewall rules.
Invoke-Expression $definition.Extent.Text

function Get-NetFirewallRule {
    [CmdletBinding()]
    param($Name)
    if ($script:FailLookup) { throw 'injected lookup failure' }
    if ($script:Existing) { [pscustomobject] @{ Name = $Name } }
}
function Save-Call($Operation, $Parameters) {
    $script:Operation = $Operation
    $script:Observed = @{} + $Parameters
    if ($script:FailWrite) { throw 'injected firewall failure' }
}
function Set-NetFirewallRule {
    [CmdletBinding()]
    param($Name, $Enabled, $Direction, $Action, $Profile, $Protocol, $LocalPort, $RemoteAddress, $Program)
    Save-Call 'set' $PSBoundParameters
}
function New-NetFirewallRule {
    [CmdletBinding()]
    param($Name, $Enabled, $Direction, $Action, $Profile, $Protocol, $LocalPort, $RemoteAddress, $Program, $DisplayName)
    Save-Call 'new' $PSBoundParameters
}

$expected = @{
    Name = 'OpenSSH-Server-In-TCP'
    Enabled = 'True'
    Direction = 'Inbound'
    Action = 'Allow'
    Profile = 'Any'
    Protocol = 'TCP'
    LocalPort = 22
    RemoteAddress = '10.0.2.2'
    Program = 'C:\Windows\System32\OpenSSH\sshd.exe'
}
$script:FailLookup = $false
foreach ($existing in @($true, $false)) {
    $script:Existing = $existing
    $script:FailWrite = $false
    $script:Operation = ''
    $script:Observed = @{}
    Set-JobSshFirewall
    $operation = if ($existing) { 'set' } else { 'new' }
    if ($script:Operation -ne $operation) { throw 'Wrong firewall mutation' }
    foreach ($key in $expected.Keys) {
        if (!$script:Observed.ContainsKey($key) -or $script:Observed[$key] -ne $expected[$key]) {
            throw "Wrong firewall setting: $key"
        }
    }
    $count = $expected.Count
    if (!$existing) { $count += 1 }
    if ($script:Observed.Count -ne $count) { throw 'Unexpected firewall settings' }

    $script:FailWrite = $true
    $failed = $false
    try { Set-JobSshFirewall } catch {
        if ($_.Exception.Message -ne 'injected firewall failure') { throw }
        $failed = $true
    }
    if (!$failed) { throw 'Firewall mutation failure was swallowed' }
}
$script:FailLookup = $true
$script:Operation = ''
$failed = $false
try { Set-JobSshFirewall } catch {
    if ($_.Exception.Message -ne 'injected lookup failure') { throw }
    $failed = $true
}
if (!$failed -or $script:Operation) { throw 'Lookup failure did not stop mutation' }
Write-Output '5 firewall policy cases passed (mocked cmdlets; not Windows runtime acceptance)'
