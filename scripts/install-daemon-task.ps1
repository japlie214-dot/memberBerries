<#
.SYNOPSIS
  Registers a per-user Scheduled Task that runs `memb.exe daemon` at logon.

.DESCRIPTION
  Nothing is registered unless you run this script yourself without -WhatIf.
  Use -WhatIf first: it builds the task definition and prints what would be registered.

  Differences from the kit's install-windows-task.ps1, and why:
   - ExecutionTimeLimit is unlimited. Windows' default stops a task after 3 days, which
     would silently end a long-running daemon.
   - MultipleInstances is IgnoreNew, so a second logon or a manual start cannot run two
     daemons (the daemon also holds a database lease, but there is no reason to start one).
   - -WhatIf is supported, and an existing task is never overwritten without -Replace.
   - The task runs as you, interactively, at the standard (not elevated) level.
   - The executable path is a required, validated parameter, not a guessed location.

  What the daemon does is described in wiki/Operations.md. It stages chat evidence,
  enforces the staging limit and optionally writes an Obsidian mirror. It runs curation
  (sending conversation text to the command in config.toml) only if you set
  curator.daemon_enabled = true yourself.

.EXAMPLE
  .\install-daemon-task.ps1 -ExePath "$env:LOCALAPPDATA\memberBerries\bin\memb.exe" -WhatIf

.EXAMPLE
  .\install-daemon-task.ps1 -ExePath C:\Tools\memb\memb.exe -MirrorOut D:\Vault\memb
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    # Absolute path of memb.exe (it must already exist).
    [Parameter(Mandatory = $true)]
    [string] $ExePath,
    # Optional existing directory for the daemon's Obsidian mirror (`daemon --mirror-out`).
    [string] $MirrorOut,
    [string] $TaskName = "MemberBerriesDaemon",
    # Overwrite an existing task of the same name.
    [switch] $Replace
)

$ErrorActionPreference = "Stop"

if (-not [System.IO.Path]::IsPathRooted($ExePath)) { throw "-ExePath must be an absolute path." }
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) { throw "memb.exe not found at the given -ExePath." }
if ((Split-Path -Leaf $ExePath) -ne "memb.exe") { throw "-ExePath must point to memb.exe." }
$arguments = "daemon"
if ($MirrorOut) {
    if (-not [System.IO.Path]::IsPathRooted($MirrorOut)) { throw "-MirrorOut must be an absolute path." }
    if (-not (Test-Path -LiteralPath $MirrorOut -PathType Container)) { throw "-MirrorOut must be an existing directory." }
    $arguments += ' --mirror-out "' + $MirrorOut + '"'
}

$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($existing -and -not $Replace) { throw "A task named '$TaskName' already exists. Use -Replace to overwrite it." }

$action = New-ScheduledTaskAction -Execute $ExePath -Argument $arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 2) `
    -StartWhenAvailable

Write-Host "Task:       $TaskName"
Write-Host "Runs as:    $user (interactive, not elevated)"
Write-Host "Command:    $ExePath $arguments"
Write-Host "Trigger:    at logon of $user"
Write-Host "Time limit: none; second instance ignored; restart up to 3 times, 2 minutes apart"

if ($PSCmdlet.ShouldProcess($TaskName, "Register scheduled task")) {
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Force:$Replace | Out-Null
    Write-Host "Registered '$TaskName'. It starts at your next logon; to start it now: Start-ScheduledTask -TaskName $TaskName"
}
