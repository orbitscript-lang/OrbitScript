# Run probe.lua under each interpreter and write one comparable report.
# Usage: .\run-probe.ps1 -Out <dir> -Interp @{ 'lua5.5'='C:\...\lua.exe'; 'lua5.4'='...'; 'orbit'='...' }
param([Parameter(Mandatory)][string]$Out, [Parameter(Mandatory)][hashtable]$Interp)
$ErrorActionPreference = 'Stop'
$probe = Join-Path $PSScriptRoot 'probe.lua'
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path $Out).Path

function Show([byte[]]$b) {
  $sb = New-Object Text.StringBuilder
  foreach ($x in $b) {
    if ($x -ge 0x20 -and $x -lt 0x7f) { [void]$sb.Append([char]$x) } else { [void]$sb.AppendFormat('<{0:x2}>', $x) }
  }
  $sb.ToString()
}

$stdinFile = Join-Path $Out 'stdin-input.bin'
[IO.File]::WriteAllBytes($stdinFile, [byte[]](0x69,0x6e,0x31,0x0d,0x0a, 0x69,0x6e,0x32,0x0a, 0x61,0x62,0x63,0x0d,0x0a, 0x72,0x65,0x73,0x74,0x1a,0x74,0x61,0x69,0x6c,0x0d,0x0a))

$summary = New-Object Collections.Generic.List[string]
foreach ($name in $Interp.Keys | Sort-Object) {
  $exe = $Interp[$name]
  $d = Join-Path $Out $name
  New-Item -ItemType Directory -Force -Path $d | Out-Null
  $summary.Add("===== $name ($exe)")
  cmd /c "`"$exe`" `"$probe`" files `"$d`" > `"$d\files.out`" 2> `"$d\files.err`""
  $summary.Add((Get-Content -Raw (Join-Path $d 'report.txt')))
  cmd /c "`"$exe`" `"$probe`" stdout > `"$d\stdout.bin`" 2> `"$d\stdout.err`""
  $summary.Add('stdout bytes = ' + (Show ([IO.File]::ReadAllBytes("$d\stdout.bin"))))
  cmd /c "`"$exe`" `"$probe`" stderr > `"$d\stderr.out`" 2> `"$d\stderr.bin`""
  $summary.Add('stderr bytes = ' + (Show ([IO.File]::ReadAllBytes("$d\stderr.bin"))))
  $summary.Add("stderr exit = $LASTEXITCODE")
  cmd /c "`"$exe`" `"$probe`" stdin `"$d`" < `"$stdinFile`" > `"$d\stdin.out`" 2> `"$d\stdin.err`""
  $summary.Add((Get-Content -Raw (Join-Path $d 'stdin.txt')))
  cmd /c "`"$exe`" `"$probe`" popen `"$d`" > `"$d\popen.out`" 2> `"$d\popen.err`""
  $summary.Add((Get-Content -Raw (Join-Path $d 'popen.txt')))
  # A script file run directly with CRLF endings and a Ctrl-Z after the code.
  [IO.File]::WriteAllBytes("$d\main.lua", [Text.Encoding]::ASCII.GetBytes("print('main')`r`nprint('after')`r`n") + [byte[]](0x1a) + [Text.Encoding]::ASCII.GetBytes("this is not lua`r`n"))
  cmd /c "`"$exe`" `"$d\main.lua`" > `"$d\main.bin`" 2> `"$d\main.err`""
  $summary.Add('main.lua with <1a> stdout = ' + (Show ([IO.File]::ReadAllBytes("$d\main.bin"))) + ' ; stderr = ' + (Show ([IO.File]::ReadAllBytes("$d\main.err"))))
  # A script from standard input (lua -) with a Ctrl-Z.
  cmd /c "`"$exe`" - < `"$d\main.lua`" > `"$d\stdinmain.bin`" 2> `"$d\stdinmain.err`""
  $summary.Add('lua - < main.lua stdout = ' + (Show ([IO.File]::ReadAllBytes("$d\stdinmain.bin"))) + ' ; stderr = ' + (Show ([IO.File]::ReadAllBytes("$d\stdinmain.err"))))
}
$summary | Set-Content -Encoding ascii (Join-Path $Out 'summary.txt')
Write-Output "Wrote $(Join-Path $Out 'summary.txt')"
