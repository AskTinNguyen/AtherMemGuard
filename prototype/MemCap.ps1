<#
.SYNOPSIS
  Put a group of apps in a Windows Job Object so they can be capped, trimmed and de-prioritized,
  leaving memory for Unreal Editor.

.DESCRIPTION
  Every app group gets one named job ("Local\MemCap-<Group>"). Processes are added to it either by
  launching an exe inside it (-Launch) or by attaching processes that are already running, by name
  (-Attach). Child processes started after that inherit the job automatically, so Electron/Chrome
  helper processes are covered.

  Controls, from gentlest to harshest:
    -MemoryPriority Low|VeryLow   Pages of these processes are evicted from RAM first. No crash risk.
    -TrimAboveMB N   (with -Watch) When the group's RAM (working set) goes over N MB, trim the working
                     sets of its processes (pushes pages to the compressed store / pagefile).
                     No crash risk; RAM only, does NOT reduce commit charge.
    -ProcessMemoryMB N            Hard commit cap per process. A process that goes over gets
                                  allocation failures, which usually means it crashes.
    -JobMemoryMB N                Hard commit cap for the whole group. Same crash risk.

  Runs as a normal user, no admin rights needed, for processes owned by the same user.

.EXAMPLE
  # Soft: Chrome pages go first, trimmed when Chrome is over 6 GB. Keeps running to catch new chrome.exe.
  .\MemCap.ps1 -Group Chrome -Attach chrome -MemoryPriority Low -TrimAboveMB 6144 -Watch

.EXAMPLE
  # Hard: launch Wispr Flow capped at 1.5 GB committed for the whole app.
  .\MemCap.ps1 -Group Wispr -Launch "$env:LOCALAPPDATA\WisprFlow\Wispr Flow.exe" -JobMemoryMB 1536

.EXAMPLE
  # Show what a group is using.
  .\MemCap.ps1 -Group Chrome -Status
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Group,
    [string]$Launch,
    [string]$Arguments = '',
    [string[]]$Attach,
    [int]$JobMemoryMB = 0,
    [int]$ProcessMemoryMB = 0,
    [ValidateSet('Normal','BelowNormal','Medium','Low','VeryLow')][string]$MemoryPriority = 'Normal',
    [int]$TrimAboveMB = 0,
    [switch]$Watch,
    [int]$IntervalSec = 15,
    [switch]$Status
)

$ErrorActionPreference = 'Stop'

if (-not ('MemCap.Native' -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
namespace MemCap {
  [StructLayout(LayoutKind.Sequential)]
  public struct BASIC_LIMIT {
    public long PerProcessUserTimeLimit; public long PerJobUserTimeLimit; public uint LimitFlags;
    public UIntPtr MinimumWorkingSetSize; public UIntPtr MaximumWorkingSetSize; public uint ActiveProcessLimit;
    public UIntPtr Affinity; public uint PriorityClass; public uint SchedulingClass;
  }
  [StructLayout(LayoutKind.Sequential)]
  public struct IO_COUNTERS { public ulong a, b, c, d, e, f; }
  [StructLayout(LayoutKind.Sequential)]
  public struct EXT_LIMIT {
    public BASIC_LIMIT Basic; public IO_COUNTERS Io;
    public UIntPtr ProcessMemoryLimit; public UIntPtr JobMemoryLimit;
    public UIntPtr PeakProcessMemoryUsed; public UIntPtr PeakJobMemoryUsed;
  }
  public static class Native {
    public const uint LIMIT_PROCESS_MEMORY = 0x100, LIMIT_JOB_MEMORY = 0x200;
    const int ExtendedLimitInformation = 9, BasicProcessIdList = 3;
    const uint JOB_ALL_ACCESS = 0x1F001F;
    const uint PROCESS_SET_QUOTA = 0x100, PROCESS_TERMINATE = 0x1, PROCESS_SET_INFORMATION = 0x200,
               PROCESS_QUERY_LIMITED = 0x1000;

    [DllImport("kernel32", SetLastError=true, CharSet=CharSet.Unicode)] static extern IntPtr CreateJobObjectW(IntPtr sa, string name);
    [DllImport("kernel32", SetLastError=true, CharSet=CharSet.Unicode)] static extern IntPtr OpenJobObjectW(uint access, bool inherit, string name);
    [DllImport("kernel32", SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job, int cls, ref EXT_LIMIT info, int len);
    [DllImport("kernel32", SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job, int cls, ref EXT_LIMIT info, int len, IntPtr ret);
    [DllImport("kernel32", SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job, int cls, IntPtr info, int len, IntPtr ret);
    [DllImport("kernel32", SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);
    [DllImport("kernel32", SetLastError=true)] static extern bool IsProcessInJob(IntPtr proc, IntPtr job, out bool result);
    [DllImport("kernel32", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32", SetLastError=true)] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32", SetLastError=true)] static extern bool SetProcessInformation(IntPtr proc, int cls, ref uint info, int len);
    [DllImport("kernel32", SetLastError=true)] static extern bool SetProcessWorkingSetSizeEx(IntPtr proc, IntPtr min, IntPtr max, uint flags);

    public static IntPtr OpenOrCreate(string name) {
      IntPtr h = OpenJobObjectW(JOB_ALL_ACCESS, false, name);
      if (h == IntPtr.Zero) h = CreateJobObjectW(IntPtr.Zero, name);
      if (h == IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
      return h;
    }
    public static IntPtr OpenExisting(string name) { return OpenJobObjectW(JOB_ALL_ACCESS, false, name); }

    public static EXT_LIMIT Query(IntPtr job) {
      EXT_LIMIT i = new EXT_LIMIT();
      if (!QueryInformationJobObject(job, ExtendedLimitInformation, ref i, Marshal.SizeOf(i), IntPtr.Zero))
        throw new System.ComponentModel.Win32Exception();
      return i;
    }
    public static void SetMemoryLimits(IntPtr job, ulong processBytes, ulong jobBytes) {
      EXT_LIMIT i = Query(job);
      i.Basic.LimitFlags &= ~(LIMIT_PROCESS_MEMORY | LIMIT_JOB_MEMORY);
      if (processBytes > 0) { i.Basic.LimitFlags |= LIMIT_PROCESS_MEMORY; i.ProcessMemoryLimit = new UIntPtr(processBytes); }
      if (jobBytes > 0)     { i.Basic.LimitFlags |= LIMIT_JOB_MEMORY;     i.JobMemoryLimit     = new UIntPtr(jobBytes); }
      if (!SetInformationJobObject(job, ExtendedLimitInformation, ref i, Marshal.SizeOf(i)))
        throw new System.ComponentModel.Win32Exception();
    }

    // Returns: 0 = added, 1 = already in this job, otherwise the Win32 error code.
    public static int Assign(IntPtr job, int pid) {
      IntPtr p = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED, false, pid);
      if (p == IntPtr.Zero) return Marshal.GetLastWin32Error();
      try {
        bool inJob;
        if (IsProcessInJob(p, job, out inJob) && inJob) return 1;
        return AssignProcessToJobObject(job, p) ? 0 : Marshal.GetLastWin32Error();
      } finally { CloseHandle(p); }
    }

    public static int[] Pids(IntPtr job) {
      int size = 8 + 8 * 4096;
      IntPtr buf = Marshal.AllocHGlobal(size);
      try {
        if (!QueryInformationJobObject(job, BasicProcessIdList, buf, size, IntPtr.Zero)) return new int[0];
        int n = Marshal.ReadInt32(buf, 4);   // NumberOfProcessIdsInList
        var list = new List<int>();
        for (int k = 0; k < n; k++) list.Add((int)Marshal.ReadInt64(buf, 8 + 8 * k));
        return list.ToArray();
      } finally { Marshal.FreeHGlobal(buf); }
    }

    // 1 = very low ... 5 = normal
    public static bool SetMemoryPriority(int pid, uint priority) {
      IntPtr p = OpenProcess(PROCESS_SET_INFORMATION, false, pid);
      if (p == IntPtr.Zero) return false;
      try { return SetProcessInformation(p, 0 /*ProcessMemoryPriority*/, ref priority, 4); }
      finally { CloseHandle(p); }
    }

    public static bool Trim(int pid) {
      IntPtr p = OpenProcess(PROCESS_SET_QUOTA, false, pid);
      if (p == IntPtr.Zero) return false;
      try { return SetProcessWorkingSetSizeEx(p, new IntPtr(-1), new IntPtr(-1), 0); }
      finally { CloseHandle(p); }
    }
  }
}
'@
}

$jobName = "Local\MemCap-$Group"
$prioMap = @{ Normal = 5; BelowNormal = 4; Medium = 3; Low = 2; VeryLow = 1 }

# Soft controls (priority, trim) act on every target process directly: sandboxed Chrome/Electron
# helpers refuse to join a job (access denied), but they do allow priority and trim.
function Get-TargetPids([IntPtr]$job) {
    $ids = @([MemCap.Native]::Pids($job))
    if ($Attach) { $ids += @(Get-Process -Name $Attach -ErrorAction SilentlyContinue | ForEach-Object Id) }
    $ids | Sort-Object -Unique
}

function Get-GroupUsage([IntPtr]$job) {
    $pids = Get-TargetPids $job
    $procs = foreach ($id in $pids) { Get-Process -Id $id -ErrorAction SilentlyContinue }
    [pscustomobject]@{
        Processes = @($procs).Count
        PrivateMB = [math]::Round((($procs | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1MB)
        RamMB     = [math]::Round((($procs | Measure-Object WorkingSet64 -Sum).Sum) / 1MB)
        Pids      = $pids
    }
}

function Write-Usage([IntPtr]$job) {
    $u = Get-GroupUsage $job
    $i = [MemCap.Native]::Query($job)
    $cap = if ($i.Basic.LimitFlags -band [MemCap.Native]::LIMIT_JOB_MEMORY) { '{0} MB' -f ($i.JobMemoryLimit.ToUInt64() / 1MB) } else { 'none' }
    $pcap = if ($i.Basic.LimitFlags -band [MemCap.Native]::LIMIT_PROCESS_MEMORY) { '{0} MB' -f ($i.ProcessMemoryLimit.ToUInt64() / 1MB) } else { 'none' }
    '{0:HH:mm:ss} [{1}] processes={2} private={3} MB ram={4} MB peak={5} MB groupCap={6} perProcessCap={7}' -f `
        (Get-Date), $Group, $u.Processes, $u.PrivateMB, $u.RamMB, [math]::Round($i.PeakJobMemoryUsed.ToUInt64() / 1MB), $cap, $pcap
}

function Add-ByName([IntPtr]$job, [string[]]$names) {
    # Joining the job only matters for hard caps.
    if ($JobMemoryMB -le 0 -and $ProcessMemoryMB -le 0) { return }
    foreach ($p in Get-Process -Name $names -ErrorAction SilentlyContinue) {
        $r = [MemCap.Native]::Assign($job, $p.Id)
        if ($r -eq 0) { Write-Verbose "added $($p.Name) $($p.Id)" }
        elseif ($r -ne 1) { Write-Warning "could not add $($p.Name) $($p.Id): Win32 error $r" }
    }
}

function Set-GroupPriority([IntPtr]$job) {
    if ($MemoryPriority -eq 'Normal') { return }
    foreach ($id in Get-TargetPids $job) { [void][MemCap.Native]::SetMemoryPriority($id, $prioMap[$MemoryPriority]) }
}

if ($Status) {
    $job = [MemCap.Native]::OpenExisting($jobName)
    if ($job -eq [IntPtr]::Zero) { "No running group '$Group'."; return }
    Write-Usage $job; return
}

$job = [MemCap.Native]::OpenOrCreate($jobName)
if ($JobMemoryMB -gt 0 -or $ProcessMemoryMB -gt 0) {
    [MemCap.Native]::SetMemoryLimits($job, [uint64]$ProcessMemoryMB * 1MB, [uint64]$JobMemoryMB * 1MB)
}

if ($Launch) {
    # Start suspended-free: the child is added right after creation. A tiny window exists where the
    # child could spawn a grandchild before being added; for normal GUI apps that does not happen.
    $psi = New-Object Diagnostics.ProcessStartInfo $Launch, $Arguments
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = Split-Path $Launch -Parent
    $proc = [Diagnostics.Process]::Start($psi)
    $r = [MemCap.Native]::Assign($job, $proc.Id)
    if ($r -gt 1) { Write-Warning "launched $($proc.Id) but could not add it: Win32 error $r" }
    else { "Launched $Launch as PID $($proc.Id) in group $Group" }
}
if ($Attach) { Add-ByName $job $Attach }
Set-GroupPriority $job
Write-Usage $job

if ($Watch) {
    while ($true) {
        Start-Sleep -Seconds $IntervalSec
        if ($Attach) { Add-ByName $job $Attach }   # catches restarts launched from outside the group
        Set-GroupPriority $job
        if ($TrimAboveMB -gt 0) {
            $u = Get-GroupUsage $job
            if ($u.RamMB -gt $TrimAboveMB) {
                foreach ($id in $u.Pids) { [void][MemCap.Native]::Trim($id) }
                "{0:HH:mm:ss} [{1}] RAM {2} MB > {3} MB, trimmed {4} processes" -f (Get-Date), $Group, $u.RamMB, $TrimAboveMB, $u.Processes
            }
        }
        Write-Usage $job
    }
}
