param(
  [Parameter(Mandatory = $true)]
  [string]$Peer,

  [string]$Exe = ".\softkvm.exe",

  [string]$Layout = "mac-left",

  [switch]$RunHost
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { "softkvm=info" }

if (!(Test-Path $Exe)) {
  throw "softkvm executable not found: $Exe"
}

$peerParts = $Peer.Split(":")
if ($peerParts.Count -ne 2) {
  throw "Peer must be host:port, got: $Peer"
}

$peerHost = $peerParts[0]
$peerPort = [int]$peerParts[1]

Write-Host "== Windows identity =="
whoami
cmd /c "query user 2>nul"

Write-Host ""
Write-Host "== Binary =="
& $Exe --help | Select-Object -First 2
if ($env:SOFTKVM_LATENCY_LOG) {
  Write-Host "Latency log: enabled"
}

Write-Host ""
Write-Host "== TCP to Mac =="
$tcp = Test-NetConnection $peerHost -Port $peerPort
$tcp | Select-Object ComputerName, RemoteAddress, TcpTestSucceeded | Format-List
if (!$tcp.TcpTestSucceeded) {
  throw "Cannot connect to $Peer. Start the Mac receiver or check firewall/LAN IP."
}

Write-Host ""
Write-Host "== Synthetic probe =="
& $Exe probe --peer $Peer

Write-Host ""
if ($RunHost) {
  Write-Host "== Real host =="
  Write-Host "Press Ctrl+C here to stop. Toggle remote mode with Ctrl+Alt+\."
  & $Exe host --peer $Peer --layout $Layout
} else {
  Write-Host "PASS: preflight probe reached Mac."
  Write-Host "Next command for real host:"
  Write-Host "& `"$Exe`" host --peer $Peer --layout $Layout"
}
