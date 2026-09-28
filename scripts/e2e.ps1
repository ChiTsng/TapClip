#Requires -Version 7
<#
.SYNOPSIS
    End-to-end test of TapClip on this machine.

.DESCRIPTION
    Starts the release build with a throw-away config (short TTL), plays the
    iPhone's part over HTTP on loopback, and drives the real Windows clipboard.
    Your clipboard text is saved first and put back at the end.
    No other TapClip instance may be running.
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\tapclip.exe'),
    [int]$Port = 47699,
    [int]$Ttl = 10
)
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class ClipProbe {
    [DllImport("user32.dll")] static extern bool OpenClipboard(IntPtr owner);
    [DllImport("user32.dll")] static extern bool CloseClipboard();
    [DllImport("user32.dll")] static extern bool EmptyClipboard();
    [DllImport("user32.dll")] static extern IntPtr SetClipboardData(uint format, IntPtr data);
    [DllImport("user32.dll")] static extern bool IsClipboardFormatAvailable(uint format);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern uint RegisterClipboardFormat(string name);
    [DllImport("user32.dll")] static extern IntPtr GetClipboardOwner();
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindow(string cls, string title);
    [DllImport("kernel32.dll")] static extern IntPtr GlobalAlloc(uint flags, UIntPtr bytes);
    [DllImport("kernel32.dll")] static extern IntPtr GlobalLock(IntPtr mem);
    [DllImport("kernel32.dll")] static extern bool GlobalUnlock(IntPtr mem);

    static IntPtr Alloc(byte[] bytes) {
        IntPtr mem = GlobalAlloc(2 /* GMEM_MOVEABLE */, (UIntPtr)bytes.Length);
        Marshal.Copy(bytes, 0, GlobalLock(mem), bytes.Length);
        GlobalUnlock(mem);
        return mem;
    }

    // Checks a format without asking the owner to render anything.
    public static bool Has(string name) { return IsClipboardFormatAvailable(RegisterClipboardFormat(name)); }

    public static bool OwnedByTapClip() {
        IntPtr window = FindWindow("TapClip.Window", null);
        return window != IntPtr.Zero && GetClipboardOwner() == window;
    }

    // Copies text the way password managers do: marked as not for monitors.
    public static void CopySensitive(string text) {
        for (int i = 0; i < 50 && !OpenClipboard(IntPtr.Zero); i++) Thread.Sleep(20);
        EmptyClipboard();
        SetClipboardData(13 /* CF_UNICODETEXT */, Alloc(Encoding.Unicode.GetBytes(text + "\0")));
        SetClipboardData(RegisterClipboardFormat("ExcludeClipboardContentFromMonitorProcessing"), Alloc(new byte[4]));
        CloseClipboard();
    }
}
'@

$Exe = (Resolve-Path $Exe).Path
$token = 'ABCD-EFGH-JKMN-PQRS'
$work = Join-Path ([IO.Path]::GetTempPath()) "tapclip-e2e-$PID"
New-Item -ItemType Directory -Path $work | Out-Null
$config = Join-Path $work 'config.toml'
Set-Content -Path $config -Encoding utf8NoBOM -Value "port = $Port`ntoken = `"$token`"`nttl_seconds = $Ttl`n"
$marker = 'ExcludeClipboardContentFromMonitorProcessing'
$script:failures = 0

function Check([string]$Name, [bool]$Ok, $Detail = '') {
    if ($Ok) { Write-Host "  PASS  $Name" -ForegroundColor Green }
    else { $script:failures++; Write-Host "  FAIL  $Name  $Detail" -ForegroundColor Red }
}

function Copy-Local([string]$Text) {
    Set-Clipboard -Value $Text
    Start-Sleep -Milliseconds 300 # let TapClip see WM_CLIPBOARDUPDATE
}

function Paste-Local { (Get-Clipboard -Raw) }

function Tap($Text, [string]$Choice, [string]$Token = $token) {
    $body = @{}
    if ($null -ne $Text) { $body.text = $Text }
    if ($Choice) { $body.choice = $Choice }
    $status = 0
    $reply = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/tap" -SkipHttpErrorCheck -StatusCodeVariable status `
        -Headers @{ Authorization = "Bearer $Token" } -ContentType 'application/json; charset=utf-8' `
        -Body ($body | ConvertTo-Json -Compress)
    $reply | Add-Member -NotePropertyName status -NotePropertyValue $status -PassThru
}

$savedClipboard = Get-Clipboard -Raw
$process = Start-Process -FilePath $Exe -ArgumentList '--config', "`"$config`"" -PassThru
try {
    $up = $false
    foreach ($i in 1..50) {
        try {
            Invoke-RestMethod "http://127.0.0.1:$Port/ping" -Headers @{ Authorization = "Bearer $token" } | Out-Null
            $up = $true; break
        } catch { Start-Sleep -Milliseconds 100 }
    }
    if (-not $up) { throw "TapClip did not come up on port $Port" }
    Write-Host "TapClip is up (pid $($process.Id)); TTL $Ttl s"

    Write-Host "`n[auth]"
    $r = Tap '' '' 'WRONG-TOKE-N000-0000'
    Check 'wrong token is rejected with 401' ($r.status -eq 401) $r.status

    Write-Host "`n[pc -> phone]"
    Copy-Local 'pc-1'
    $r = Tap ''
    Check 'fresh PC copy is handed to the phone' ($r.copy -eq 'pc-1' -and $r.message.StartsWith('⬇')) ($r | ConvertTo-Json -Compress)
    $r = Tap 'pc-1'
    Check 'same text on both sides is in sync' ($r.message -eq '两边已经是同一段内容' -and $null -eq $r.copy) $r.message

    Write-Host "`n[phone -> pc, pasted]"
    Copy-Local 'orig-2'
    Tap $null | Out-Null # the phone takes orig-2, so the PC side is no longer fresh
    $r = Tap 'phone-2'
    Check 'new phone text is sent' ($r.message.StartsWith('⬆')) $r.message
    Check 'offer is owned by TapClip' ([ClipProbe]::OwnedByTapClip())
    Check 'offer is hidden from history and monitors' ([ClipProbe]::Has($marker))
    Check 'pasting yields the phone text' ((Paste-Local) -eq 'phone-2') (Paste-Local)
    Start-Sleep -Milliseconds 500
    Check 'pasted offer is promoted to a normal item' (-not [ClipProbe]::Has($marker))
    Check 'promoted text stays on the clipboard' ((Paste-Local) -eq 'phone-2') (Paste-Local)

    Write-Host "`n[phone -> pc, expires unused]"
    Copy-Local 'orig-3'
    Tap $null | Out-Null
    $r = Tap 'phone-3'
    Check 'phone text is sent' ($r.message.StartsWith('⬆')) $r.message
    Start-Sleep -Seconds ($Ttl + 2)
    Check 'expired offer restores the previous clipboard' ((Paste-Local) -eq 'orig-3') (Paste-Local)
    $r = Tap 'phone-3'
    Check 'tapping again re-sends the same phone text' ($r.message.StartsWith('⬆')) $r.message

    Write-Host "`n[local copy during an offer]"
    Copy-Local 'local-4'
    Start-Sleep -Seconds ($Ttl + 2)
    Check 'a new local copy wins and is not rolled back' ((Paste-Local) -eq 'local-4') (Paste-Local)

    Write-Host "`n[both sides new]"
    Copy-Local 'pc-5'
    $r = Tap 'phone-5'
    Check 'conflict asks the phone' ($r.ask -eq '1') ($r | ConvertTo-Json -Compress)
    $r = Tap 'phone-5' 'receive'
    Check 'choosing receive hands over the PC text' ($r.copy -eq 'pc-5') ($r | ConvertTo-Json -Compress)
    Copy-Local 'pc-6'
    Tap 'phone-6' | Out-Null
    $r = Tap 'phone-6' 'send'
    Check 'choosing send offers the phone text' ($r.message.StartsWith('⬆') -and (Paste-Local) -eq 'phone-6') (Paste-Local)

    Write-Host "`n[sensitive content]"
    [ClipProbe]::CopySensitive('secret-7')
    Start-Sleep -Milliseconds 300
    $r = Tap $null
    Check 'marked-sensitive copy is never handed out' ($null -eq $r.copy -and $r.message -eq '没有新内容') ($r | ConvertTo-Json -Compress)

    Write-Host "`n[line endings]"
    Copy-Local "line1`r`nline2"
    $r = Tap $null
    Check 'CRLF becomes LF for the phone' ($r.copy -eq "line1`nline2") ($r.copy | ConvertTo-Json)
    Tap "x`ny" | Out-Null
    Check 'LF becomes CRLF on the PC' ((Paste-Local) -eq "x`r`ny") ((Paste-Local) | ConvertTo-Json)

    Write-Host "`n[footprint]"
    $process.Refresh()
    $cpuBefore = $process.TotalProcessorTime
    Start-Sleep -Seconds 3
    $process.Refresh()
    $idleMs = ($process.TotalProcessorTime - $cpuBefore).TotalMilliseconds
    $memMb = [math]::Round($process.PrivateMemorySize64 / 1MB, 1)
    Check "idle CPU over 3 s: $idleMs ms" ($idleMs -lt 20)
    Check "private memory: $memMb MB" ($memMb -lt 10)
}
finally {
    & $Exe --quit
    if (-not $process.WaitForExit(3000)) { Stop-Process -Id $process.Id -Force }
    if ($null -ne $savedClipboard) { Set-Clipboard -Value $savedClipboard }
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:failures -eq 0) { Write-Host 'All end-to-end checks passed.' -ForegroundColor Green; exit 0 }
Write-Host "$script:failures check(s) failed. Log: $env:LOCALAPPDATA\TapClip\tapclip.log" -ForegroundColor Red
exit 1
