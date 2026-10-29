[CmdletBinding()]
param([string]$InterfaceAlias = 'Ethernet')

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell on a trusted private LAN.'
}
$profile = Get-NetConnectionProfile -InterfaceAlias $InterfaceAlias
if ($profile.NetworkCategory -ne 'Private') {
    throw "Interface '$InterfaceAlias' is not Private. Verify that the LAN is trusted and change its profile yourself before adding the rule."
}
$name = 'LlmQuotaMonitor8765'
if (Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue) {
    throw "Rule '$name' already exists. Inspect it instead of creating a duplicate."
}
New-NetFirewallRule -Name $name -DisplayName 'LLM Quota Monitor - private LAN' `
    -Direction Inbound -Action Allow -Protocol TCP -LocalPort 8765 `
    -Profile Private -RemoteAddress LocalSubnet -InterfaceAlias $InterfaceAlias
