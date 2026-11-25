<#
.SYNOPSIS
  Stops and removes the Scheduled Task created by install-daemon-task.ps1.
  Use -WhatIf to see what would happen. Your notes and data directory are not touched.
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Medium")]
param(
    [string] $TaskName = "MemberBerriesDaemon"
)

$ErrorActionPreference = "Stop"
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) {
    Write-Host "No task named '$TaskName' is registered; nothing to do."
    return
}
if ($PSCmdlet.ShouldProcess($TaskName, "Stop and unregister scheduled task")) {
    if ($task.State -eq "Running") { Stop-ScheduledTask -TaskName $TaskName }
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Removed '$TaskName'."
}
