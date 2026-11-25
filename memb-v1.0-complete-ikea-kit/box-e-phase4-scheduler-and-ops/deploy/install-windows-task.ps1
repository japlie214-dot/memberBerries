$ErrorActionPreference = "Stop"
$Exe = "$env:LOCALAPPDATA\memberBerries\bin\memb.exe"
if (-not (Test-Path $Exe)) { Write-Error "Build release first and copy memb.exe to $Exe" }
$Action = New-ScheduledTaskAction -Execute $Exe -Argument "daemon"
$Trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 2)
Register-ScheduledTask -TaskName "MemberBerriesDaemon" -Action $Action -Trigger $Trigger -Settings $Settings -Force
Write-Host "Installed Scheduled Task: MemberBerriesDaemon"
