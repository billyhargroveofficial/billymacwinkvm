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
$env:SOFTKVM_MOTION_TRANSPORT = if ($env:SOFTKVM_MOTION_TRANSPORT) { $env:SOFTKVM_MOTION_TRANSPORT } else { "udp" }
$env:SOFTKVM_UDP_SEND_MODE = if ($env:SOFTKVM_UDP_SEND_MODE) { $env:SOFTKVM_UDP_SEND_MODE } else { "immediate" }

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
& $Exe build-info
if ($env:SOFTKVM_LATENCY_LOG) {
  Write-Host "Latency log: enabled"
}
if ($env:SOFTKVM_MOTION_TRANSPORT -eq "tcp") {
  Write-Host "Motion transport: forced tcp/json fallback"
} else {
  Write-Host "Motion transport: udp/binary on the same peer port"
  Write-Host "UDP send mode: $env:SOFTKVM_UDP_SEND_MODE"
}

Write-Host ""
Write-Host "== softkvm protocol probe =="
& $Exe probe --peer $Peer
if ($LASTEXITCODE -ne 0) {
  throw "softkvm protocol probe failed for $Peer (exit $LASTEXITCODE). The error above includes the WinSock code."
}

Write-Host ""
if ($RunHost) {
  Write-Host "== Real host =="
  Write-Host "Press Ctrl+C here to stop. Toggle remote mode with Ctrl+Alt+\."
  & $Exe host --peer $Peer --layout $Layout
  if ($LASTEXITCODE -ne 0) {
    throw "softkvm host exited with code $LASTEXITCODE."
  }
} else {
  Write-Host "PASS: preflight probe reached Mac."
  Write-Host "Next command for real host:"
  Write-Host "& `"$Exe`" host --peer $Peer --layout $Layout"
}
