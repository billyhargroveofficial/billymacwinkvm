# Diagnose outbound TcpStream::connect failing with WSAEADDRINUSE (os error
# 10048, kind=AddrInUse) — as seen after Defender interfered with softkvm.exe.
#
# An OUTBOUND connect returning 10048 means Windows could not allocate a local
# ephemeral port for the socket. Realistic causes, in observed-likelihood order:
#   1. Port-exclusion ranges (Hyper-V/WSL2/WinNAT or `netsh ... add
#      excludedportrange`) swallowing most of the dynamic range. Very common
#      after an update/reboot storm; Defender remediation often coincides with
#      the reboot that reshuffles WinNAT reservations.
#   2. Shrunken/misconfigured dynamic port range (netsh set dynamicport).
#   3. Ephemeral exhaustion by TIME_WAIT floods or a leaking process
#      (thousands of sockets), including duplicate softkvm instances.
#   4. A Winsock LSP/WFP callout (security software) failing allocations.
# Note: 10048 is NOT the Mac's 49321 being busy — that would be 10061
# (refused) or 10060 (timeout); the local endpoint fails before any packet
# leaves the machine.
#
# Run:  powershell -ExecutionPolicy Bypass -File .\diagnose-addrinuse.ps1
#       [-Peer 192.168.1.11] [-Deep]

param(
    [string]$Peer = "192.168.1.11",
    [switch]$Deep
)

$ErrorActionPreference = "Continue"

Write-Host "== 1. softkvm processes (duplicates keep sockets alive) =="
Get-Process | Where-Object { $_.ProcessName -like "*softkvm*" } |
    Format-Table Id, ProcessName, StartTime, CPU -AutoSize
Write-Host ""

Write-Host "== 2. TCP connection states (TIME_WAIT floods exhaust ephemeral ports) =="
$conns = Get-NetTCPConnection
$conns | Group-Object State | Sort-Object Count -Descending |
    Format-Table Count, Name -AutoSize
$toPeer = $conns | Where-Object { $_.RemoteAddress -eq $Peer }
Write-Host ("connections toward {0}: {1}" -f $Peer, ($toPeer | Measure-Object).Count)
$toPeer | Group-Object State | Format-Table Count, Name -AutoSize
Write-Host ""

Write-Host "== 3. Dynamic (ephemeral) port range =="
netsh int ipv4 show dynamicport tcp
Write-Host ""

Write-Host "== 4. Excluded port ranges (Hyper-V/WSL2 reservations live here) =="
Write-Host "   If these blanket 49152-65535, ephemeral allocation fails -> 10048."
netsh int ipv4 show excludedportrange protocol=tcp
Write-Host ""

Write-Host "== 5. Ephemeral allocation self-test (100 outbound binds) =="
$failed = 0
$sockets = @()
for ($i = 0; $i -lt 100; $i++) {
    try {
        $s = New-Object System.Net.Sockets.Socket(
            [System.Net.Sockets.AddressFamily]::InterNetwork,
            [System.Net.Sockets.SocketType]::Stream,
            [System.Net.Sockets.ProtocolType]::Tcp)
        $s.Bind((New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 0)))
        $sockets += $s
    } catch {
        $failed++
    }
}
$sockets | ForEach-Object { $_.Close() }
Write-Host ("bind(0.0.0.0:0) x100: {0} failed" -f $failed)
if ($failed -gt 0) {
    Write-Host ">>> ephemeral allocation IS broken machine-wide (matches 10048)."
} else {
    Write-Host ">>> ephemeral allocation works now; if softkvm still fails, suspect a"
    Write-Host "    per-process WFP/LSP rule targeting softkvm.exe (see step 7)."
}
Write-Host ""

Write-Host "== 6. Defender history touching softkvm =="
Get-MpThreatDetection 2>$null |
    Sort-Object InitialDetectionTime -Descending | Select-Object -First 10 |
    Format-Table InitialDetectionTime, ProcessName, Resources -AutoSize
Get-WinEvent -LogName "Microsoft-Windows-Windows Defender/Operational" -MaxEvents 400 2>$null |
    Where-Object { $_.Message -match "softkvm" } | Select-Object -First 10 |
    Format-Table TimeCreated, Id, LevelDisplayName -AutoSize
Write-Host "(consider: Add-MpPreference -ExclusionPath <kit folder> — security tradeoff:"
Write-Host " the folder stops being scanned; acceptable for a dev-only test kit you built.)"
Write-Host ""

if ($Deep) {
    Write-Host "== 7. WFP state snapshot (deep) =="
    netsh wfp show state file=$env:TEMP\wfpstate.xml | Out-Null
    Write-Host "wrote $env:TEMP\wfpstate.xml — search it for 'softkvm' and for provider"
    Write-Host "names of security products; per-app block/inspect filters show up there."
    Write-Host ""
    Write-Host "== 8. Winsock catalog (LSPs) =="
    netsh winsock show catalog | Select-String -Pattern "Description|Provider" | Select-Object -First 30
}

Write-Host ""
Write-Host "== Interpretation =="
Write-Host "* excludedportrange covers most of 49152-65535  -> `netsh int ipv4 delete"
Write-Host "  excludedportrange` the offenders or reboot; for WinNAT: `net stop winnat;"
Write-Host "  net start winnat` reshuffles reservations."
Write-Host "* dynamicport range tiny (num < 1000)           -> netsh int ipv4 set dynamicport"
Write-Host "  tcp start=49152 num=16384"
Write-Host "* TIME_WAIT count in thousands                  -> find the leaking process"
Write-Host "  (Get-NetTCPConnection | Group-Object OwningProcess), fix or wait 30-240 s."
Write-Host "* duplicate softkvm.exe                         -> Stop-Process, rerun once."
Write-Host "* self-test clean but softkvm alone fails       -> Defender/WFP per-app rule;"
Write-Host "  add the exclusion above or inspect wfpstate.xml."
