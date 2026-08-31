# Capture ALVR's own log lines on the headset itself, then pull the file.
#
# Filtering and writing both happen on-device, so neither the Huawei SDK's log
# spam nor an interrupted adb connection can truncate the capture.
#
#   .\capture-alvr-log.ps1 start   # begin capturing, then reproduce on the headset
#   .\capture-alvr-log.ps1 stop    # stop and pull to logs\alvr.txt
#
# The headset address defaults to $env:ALVR_DEVICE, or pass -Device <host:port>.
# adb is taken from PATH, or from $env:ANDROID_HOME\platform-tools.
param(
    [ValidateSet('start', 'stop')]
    [string]$Action = 'start',
    [string]$Device = $(if ($env:ALVR_DEVICE) { $env:ALVR_DEVICE } else { '192.168.1.1:5555' }),
    [string]$Out = 'logs\alvr.txt'
)

$adb = if ($env:ANDROID_HOME -and (Test-Path "$env:ANDROID_HOME\platform-tools\adb.exe")) {
    "$env:ANDROID_HOME\platform-tools\adb.exe"
}
else {
    'adb'
}
$remote = '/sdcard/alvr-capture.txt'

& $adb connect $Device | Out-Null
& $adb -s $Device shell pkill -f logcat | Out-Null

if ($Action -eq 'start') {
    & $adb -s $Device shell rm -f $remote | Out-Null
    & $adb -s $Device logcat -c -b main,crash,system
    # Keep the pattern free of spaces: adb strips one quoting level before the remote sh sees it.
    $pattern = 'ALVR.NATIVE-RUST|FATAL|panicked|beginning.of.crash|Force.finishing|died'
    Start-Process -FilePath $adb -WindowStyle Hidden -ArgumentList @(
        '-s', $Device, 'shell',
        "logcat -v time -b main,crash,system | grep --line-buffered -a -E '$pattern' > $remote"
    )
    Write-Host "Capturing to $remote. Reproduce now, then run: .\capture-alvr-log.ps1 stop"
}
else {
    New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
    & $adb -s $Device pull $remote $Out
    Write-Host "Pulled $((Get-Item $Out).Length) bytes to $Out"
}
