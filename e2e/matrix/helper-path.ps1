param([ValidateSet('e2e', 'installed', 'show')][string]$To = 'show')
# Points beta's LumepeerHelper service at the e2e build (so its LocalSystem
# desktop injector runs the code under test) or back at the installed one.
$ErrorActionPreference = 'Stop'
$paths = @{
    e2e       = '"C:\Users\bberb\AppData\Local\lumepeer-e2e\app\lumepeer-service.exe"'
    installed = '"C:\Program Files\Lumepeer\lumepeer-service.exe"'
}
$svc = Get-CimInstance Win32_Service -Filter "Name='LumepeerHelper'"
if ($To -ne 'show') {
    Stop-Service LumepeerHelper -Force
    Start-Sleep -Seconds 1
    # The desktop injector the service started goes with it; one left behind
    # would keep the old code attached to nothing.
    Get-CimInstance Win32_Process -Filter "Name='lumepeer-desktop.exe'" |
        Where-Object { $_.CommandLine -like '*--system-input-worker*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
    $r = Invoke-CimMethod -InputObject $svc -MethodName Change -Arguments @{ PathName = $paths[$To] }
    "Change returned $($r.ReturnValue)"
    Start-Service LumepeerHelper
    Start-Sleep -Seconds 1
    $svc = Get-CimInstance Win32_Service -Filter "Name='LumepeerHelper'"
}
"{0} {1} {2}" -f $svc.Name, $svc.State, $svc.PathName
