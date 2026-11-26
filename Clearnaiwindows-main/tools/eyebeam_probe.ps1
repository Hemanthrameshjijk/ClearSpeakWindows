<#
eyeBeam probe: read-only. Shows what ClearSpeak could read from eyeBeam during a call, so automatic call recording
can start and stop on the right signal and name each file with the caller's number.

What it records (to the screen and to a text file on the Desktop):
  1. eyeBeam's program path and version.
  2. eyeBeam / CounterPath folders under AppData and Program Files (file names, sizes, times).
  3. Every change in eyeBeam's windows, once every half second: window titles, buttons, labels, text (the call
     panel's number, state and timer if Windows can see them). Also the browser window showing the VICIdial agent
     screen, if open: keep VICIdial on the visible tab during the test (ClearSpeak's "VICIdial agent screen" method
     reads its "Live Call" mark and phone_number field).
  4. When eyeBeam opens and closes the microphone (tells whether it holds the mic all shift, as with VICIdial).
  5. At the end: which files under those folders changed during the test (eyeBeam's call history).

It changes nothing: no clicks, no settings, no network. Stop it any time with Ctrl+C.

How to run (on an agent PC, during a TEST call, not a real customer):
  powershell -ExecutionPolicy Bypass -File eyebeam_probe.ps1            (runs 3 minutes)
  powershell -ExecutionPolicy Bypass -File eyebeam_probe.ps1 -Seconds 300

While it runs, do this in eyeBeam and say the time of each step out loud or note it:
  log in to VICIdial (if used) -> take a call -> Answer -> Hold -> Resume -> Hang up
  -> a second call -> Decline. Then send back the file it names at the end.
The file can contain the test caller's number.
#>
# -Match: which programs to watch (a regex on the process name); only change it to test the probe on another app.
param([int]$Seconds = 180, [string]$Match = 'eyebeam|xlite|x-lite|counterpath|bria')

$ErrorActionPreference = 'SilentlyContinue'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$started = Get-Date
$outFile = Join-Path ([Environment]::GetFolderPath('Desktop')) ("eyebeam-probe-" + $started.ToString('yyyy-MM-dd_HH-mm-ss') + ".txt")

function Write-Probe([string]$line) {
    $t = '{0,6:N1}s' -f ((Get-Date) - $started).TotalSeconds
    $l = "[$t] $line"
    Write-Host $l
    Add-Content -Path $outFile -Value $l -Encoding UTF8
}

$namePattern = $Match

Write-Probe "eyeBeam probe started on $env:COMPUTERNAME, Windows $([Environment]::OSVersion.Version), for $Seconds s"

# --- 1. The program ---
$procs = @(Get-Process | Where-Object { $_.ProcessName -match $namePattern })
if ($procs.Count -eq 0) {
    Write-Probe "eyeBeam is NOT running. Start eyeBeam first, then run this again."
} else {
    foreach ($p in $procs) {
        $v = $p.MainModule.FileVersionInfo
        Write-Probe ("program: {0} (pid {1}) {2} version {3} / {4}" -f $p.ProcessName, $p.Id, $p.Path, $v.FileVersion, $v.ProductVersion)
    }
}

# --- 2. Folders ---
$roots = @($env:APPDATA, $env:LOCALAPPDATA, ${env:ProgramFiles(x86)}, $env:ProgramFiles, "$env:USERPROFILE\Documents") | Where-Object { $_ }
$dirs = @()
foreach ($r in $roots) {
    $dirs += Get-ChildItem $r -Directory -Depth 1 | Where-Object { $_.Name -match $namePattern }
}
$dirs = $dirs | Sort-Object FullName -Unique
$before = @{}
if ($dirs.Count -eq 0) { Write-Probe "no eyeBeam/CounterPath folders found under AppData, Program Files or Documents" }
foreach ($d in $dirs) {
    Write-Probe "folder: $($d.FullName)"
    foreach ($f in Get-ChildItem $d.FullName -File -Recurse -Depth 5) {
        $before[$f.FullName] = $f.LastWriteTime
        Write-Probe ("   {0}  {1:N0} bytes  {2}" -f $f.FullName.Substring($d.FullName.Length), $f.Length, $f.LastWriteTime)
    }
}

# --- 3./4. Watch windows and the microphone ---
$micKeys = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged'
function Get-MicState {
    $on = @()
    foreach ($k in Get-ChildItem $micKeys) {
        if ($k.PSChildName -notmatch $namePattern) { continue }
        $p = Get-ItemProperty $k.PSPath
        if ($p.LastUsedTimeStart -gt 0 -and $p.LastUsedTimeStop -eq 0) { $on += ($k.PSChildName -split '#')[-1] }
    }
    if ($on.Count) { 'IN USE by ' + ($on -join ', ') } else { 'free' }
}

$walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
function Get-Lines($el, [int]$depth, [System.Collections.Generic.List[string]]$out) {
    if ($null -eq $el -or $depth -gt 12 -or $out.Count -gt 800) { return }
    $c = $el.Current
    $line = ('  ' * $depth) + $c.ControlType.ProgrammaticName.Replace('ControlType.', '') +
        " name='" + $c.Name + "'"
    if ($c.AutomationId) { $line += " id='" + $c.AutomationId + "'" }
    if ($c.ClassName) { $line += " class='" + $c.ClassName + "'" }
    if (-not $c.IsEnabled) { $line += ' (disabled)' }
    if ($c.IsOffscreen) { $line += ' (hidden)' }
    $vp = $null
    if ($el.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$vp)) {
        $line += " value='" + $vp.Current.Value + "'"
    }
    $tp = $null
    if ($c.ControlType -ne [System.Windows.Automation.ControlType]::Window -and
        $el.TryGetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern, [ref]$tp)) {
        $txt = $tp.DocumentRange.GetText(200) -replace '\s+', ' '
        if ($txt) { $line += " text='" + $txt + "'" }
    }
    $out.Add($line)
    $child = $walker.GetFirstChild($el)
    while ($null -ne $child) {
        Get-Lines $child ($depth + 1) $out
        $child = $walker.GetNextSibling($child)
    }
}

function Get-Snapshot {
    $out = New-Object 'System.Collections.Generic.List[string]'
    $ids = @(Get-Process | Where-Object { $_.ProcessName -match $namePattern } | ForEach-Object { $_.Id })
    if ($ids.Count -eq 0) { $out.Add('(eyeBeam not running)') }
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $win = $walker.GetFirstChild($root)
    while ($null -ne $win) {
        # eyeBeam's windows, and the browser window showing the VICIdial agent screen (its visible tab).
        if ($ids -contains $win.Current.ProcessId -or $win.Current.Name -match 'vicidial|agent web client') { Get-Lines $win 0 $out }
        $win = $walker.GetNextSibling($win)
    }
    if ($out.Count -eq 0) { $out.Add('(eyeBeam has no window open)') }
    $out
}

$prev = @()
$prevMic = ''
$first = $true
$end = $started.AddSeconds($Seconds)
while ((Get-Date) -lt $end) {
    $mic = Get-MicState
    if ($mic -ne $prevMic) { Write-Probe "microphone: $mic"; $prevMic = $mic }

    $snap = @(Get-Snapshot)
    if ($first) {
        Write-Probe "eyeBeam windows now ($($snap.Count) items):"
        foreach ($l in $snap) { Write-Probe "   $l" }
        $first = $false
    } else {
        $gone = $prev | Where-Object { $snap -notcontains $_ }
        $new = $snap | Where-Object { $prev -notcontains $_ }
        if ($gone -or $new) {
            Write-Probe 'eyeBeam window changed:'
            foreach ($l in $gone) { Write-Probe "   - $l" }
            foreach ($l in $new) { Write-Probe "   + $l" }
        }
    }
    $prev = $snap
    Start-Sleep -Milliseconds 500
}

# --- 5. Files that changed during the test ---
Write-Probe 'files under the eyeBeam folders changed during the test:'
$any = $false
foreach ($d in $dirs) {
    foreach ($f in Get-ChildItem $d.FullName -File -Recurse -Depth 5) {
        if (-not $before.ContainsKey($f.FullName) -or $before[$f.FullName] -ne $f.LastWriteTime) {
            Write-Probe ("   {0}  {1:N0} bytes  {2}" -f $f.FullName, $f.Length, $f.LastWriteTime)
            $any = $true
        }
    }
}
if (-not $any) { Write-Probe '   (none)' }
Write-Probe "done. Please send this file: $outFile"
