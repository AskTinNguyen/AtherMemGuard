// MemCap: keep background apps from crowding out Unreal Editor.
// Lowers memory priority of chosen apps, trims their RAM over a budget, and optionally hard-caps
// commit with a Job Object. Tray app, per-user, no admin rights needed.
// Build: csc /target:winexe /out:MemCap.exe MemCap.cs   (.NET Framework 4.x, C# 5)
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Windows.Forms;
using Microsoft.Win32;

[assembly: AssemblyTitle("MemCap")]
[assembly: AssemblyProduct("MemCap")]
[assembly: AssemblyDescription("Keeps background apps from crowding out Unreal Editor's memory")]
[assembly: AssemblyVersion("1.0.0.0")]
[assembly: AssemblyFileVersion("1.0.0.0")]

namespace MemCap
{
    class Group
    {
        public string Name;
        public string[] Processes = new string[0];
        public int Priority = 5;          // 1 very low .. 5 normal
        public long TrimAboveMB;          // 0 = never trim
        public long JobMemoryMB;          // 0 = no hard cap (whole group)
        public long ProcessMemoryMB;      // 0 = no hard cap (per process)
        public IntPtr Job = IntPtr.Zero;
        public string LastStatus = "";
        public long LastRamMB;
    }

    static class Native
    {
        [StructLayout(LayoutKind.Sequential)]
        public struct BASIC_LIMIT
        {
            public long PerProcessUserTimeLimit; public long PerJobUserTimeLimit; public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize; public UIntPtr MaximumWorkingSetSize; public uint ActiveProcessLimit;
            public UIntPtr Affinity; public uint PriorityClass; public uint SchedulingClass;
        }
        [StructLayout(LayoutKind.Sequential)]
        public struct IO_COUNTERS { public ulong a, b, c, d, e, f; }
        [StructLayout(LayoutKind.Sequential)]
        public struct EXT_LIMIT
        {
            public BASIC_LIMIT Basic; public IO_COUNTERS Io;
            public UIntPtr ProcessMemoryLimit; public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed; public UIntPtr PeakJobMemoryUsed;
        }

        [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)] public static extern IntPtr CreateJobObjectW(IntPtr sa, string name);
        [DllImport("kernel32", SetLastError = true)] public static extern bool SetInformationJobObject(IntPtr job, int cls, ref EXT_LIMIT info, int len);
        [DllImport("kernel32", SetLastError = true)] public static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);
        [DllImport("kernel32", SetLastError = true)] public static extern bool IsProcessInJob(IntPtr proc, IntPtr job, out bool result);
        [DllImport("kernel32", SetLastError = true)] public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
        [DllImport("kernel32", SetLastError = true)] public static extern bool CloseHandle(IntPtr h);
        [DllImport("kernel32", SetLastError = true)] public static extern bool SetProcessInformation(IntPtr proc, int cls, ref uint info, int len);
        [DllImport("kernel32", SetLastError = true)] public static extern bool SetProcessWorkingSetSizeEx(IntPtr proc, IntPtr min, IntPtr max, uint flags);

        const uint PROCESS_SET_QUOTA = 0x100, PROCESS_TERMINATE = 0x1, PROCESS_SET_INFORMATION = 0x200, PROCESS_QUERY_LIMITED = 0x1000;
        public const uint LIMIT_PROCESS_MEMORY = 0x100, LIMIT_JOB_MEMORY = 0x200;

        public static bool SetMemoryPriority(int pid, uint priority)
        {
            IntPtr p = OpenProcess(PROCESS_SET_INFORMATION, false, pid);
            if (p == IntPtr.Zero) return false;
            try { return SetProcessInformation(p, 0 /* ProcessMemoryPriority */, ref priority, 4); }
            finally { CloseHandle(p); }
        }

        public static bool Trim(int pid)
        {
            IntPtr p = OpenProcess(PROCESS_SET_QUOTA, false, pid);
            if (p == IntPtr.Zero) return false;
            try { return SetProcessWorkingSetSizeEx(p, new IntPtr(-1), new IntPtr(-1), 0); }
            finally { CloseHandle(p); }
        }

        public static IntPtr CreateCappedJob(string name, long processMB, long jobMB)
        {
            IntPtr job = CreateJobObjectW(IntPtr.Zero, name);
            if (job == IntPtr.Zero) throw new Win32Exception();
            EXT_LIMIT i = new EXT_LIMIT();
            if (processMB > 0) { i.Basic.LimitFlags |= LIMIT_PROCESS_MEMORY; i.ProcessMemoryLimit = new UIntPtr((ulong)processMB * 1048576UL); }
            if (jobMB > 0) { i.Basic.LimitFlags |= LIMIT_JOB_MEMORY; i.JobMemoryLimit = new UIntPtr((ulong)jobMB * 1048576UL); }
            if (!SetInformationJobObject(job, 9, ref i, Marshal.SizeOf(i))) throw new Win32Exception();
            return job;
        }

        // true if added or already in the job
        public static bool Assign(IntPtr job, int pid)
        {
            IntPtr p = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED, false, pid);
            if (p == IntPtr.Zero) return false;
            try
            {
                bool inJob;
                if (IsProcessInJob(p, job, out inJob) && inJob) return true;
                return AssignProcessToJobObject(job, p);
            }
            finally { CloseHandle(p); }
        }
    }

    static class Paths
    {
        public static readonly string DataDir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "MemCap");
        public static readonly string InstallDir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "MemCap");
        public static readonly string Config = Path.Combine(DataDir, "memcap.ini");
        public static readonly string Log = Path.Combine(DataDir, "memcap.log");
        public const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
        public const string ExitEvent = @"Local\MemCap-Exit";
        public const string Mutex = @"Local\MemCap-Running";
    }

    static class Log
    {
        static readonly object Gate = new object();
        public static void Write(string msg)
        {
            lock (Gate)
            {
                try
                {
                    Directory.CreateDirectory(Paths.DataDir);
                    FileInfo f = new FileInfo(Paths.Log);
                    if (f.Exists && f.Length > 2 * 1024 * 1024) File.Copy(Paths.Log, Paths.Log + ".old", true);
                    if (f.Exists && f.Length > 2 * 1024 * 1024) File.Delete(Paths.Log);
                    File.AppendAllText(Paths.Log, DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss") + "  " + msg + Environment.NewLine);
                }
                catch { }
            }
        }
    }

    static class Config
    {
        public const string Default =
@"# MemCap settings. Changes are picked up automatically within one interval.
#
# Each [section] is a group of apps.
#   processes       = process names without .exe, comma separated
#   priority        = Normal | BelowNormal | Medium | Low | VeryLow
#                     Windows evicts pages of low-priority apps from RAM first. No crash risk.
#   trimAboveMB     = when the group uses more RAM than this, push its memory to the compressed
#                     store / page file. No crash risk. 0 = off. Frees RAM, not commit charge.
#   jobMemoryMB     = HARD cap on committed memory for the whole group. Apps that hit it usually
#                     crash. 0 = off. Sandboxed Chrome/Electron helpers cannot be capped.
#   processMemoryMB = HARD cap per process. Same warning. 0 = off.
#
# Do not list Unreal tools here (UnrealEditor, dotnet, UnrealBuildTool, cl, link, git used by builds).

[settings]
intervalSec = 15

[Chrome]
processes   = chrome, msedge
priority    = Low
trimAboveMB = 4096

[Chat]
processes   = Slack, ChatGPT, Wispr Flow, Discord, Teams, ms-teams
priority    = VeryLow
trimAboveMB = 1536

[Claude]
processes   = claude
priority    = Low
";

        public static int IntervalSec = 15;

        public static List<Group> Load()
        {
            Directory.CreateDirectory(Paths.DataDir);
            if (!File.Exists(Paths.Config)) File.WriteAllText(Paths.Config, Default);
            List<Group> groups = new List<Group>();
            Group cur = null; bool inSettings = false;
            IntervalSec = 15;
            foreach (string raw in File.ReadAllLines(Paths.Config))
            {
                string line = raw.Trim();
                int hash = line.IndexOf('#');
                if (hash >= 0) line = line.Substring(0, hash).Trim();
                if (line.Length == 0) continue;
                if (line.StartsWith("[") && line.EndsWith("]"))
                {
                    string name = line.Substring(1, line.Length - 2).Trim();
                    inSettings = name.Equals("settings", StringComparison.OrdinalIgnoreCase);
                    cur = null;
                    if (!inSettings) { cur = new Group(); cur.Name = name; groups.Add(cur); }
                    continue;
                }
                int eq = line.IndexOf('=');
                if (eq < 0) continue;
                string key = line.Substring(0, eq).Trim().ToLowerInvariant();
                string val = line.Substring(eq + 1).Trim();
                if (inSettings)
                {
                    if (key == "intervalsec") IntervalSec = Math.Max(5, ParseInt(val));
                    continue;
                }
                if (cur == null) continue;
                switch (key)
                {
                    case "processes":
                        List<string> names = new List<string>();
                        foreach (string n in val.Split(','))
                        {
                            string t = n.Trim();
                            if (t.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) t = t.Substring(0, t.Length - 4);
                            if (t.Length > 0) names.Add(t);
                        }
                        cur.Processes = names.ToArray();
                        break;
                    case "priority": cur.Priority = ParsePriority(val); break;
                    case "trimabovemb": cur.TrimAboveMB = ParseInt(val); break;
                    case "jobmemorymb": cur.JobMemoryMB = ParseInt(val); break;
                    case "processmemorymb": cur.ProcessMemoryMB = ParseInt(val); break;
                }
            }
            return groups;
        }

        static int ParseInt(string s) { int v; return int.TryParse(s, out v) ? Math.Max(0, v) : 0; }

        static int ParsePriority(string s)
        {
            switch (s.Trim().ToLowerInvariant())
            {
                case "verylow": return 1;
                case "low": return 2;
                case "medium": return 3;
                case "belownormal": return 4;
                default: return 5;
            }
        }
    }

    class Engine
    {
        List<Group> groups = new List<Group>();
        DateTime configStamp = DateTime.MinValue;
        public bool Paused;

        public void ReloadIfChanged(bool force)
        {
            try
            {
                if (!File.Exists(Paths.Config)) { Config.Load(); force = true; }
                DateTime stamp = File.GetLastWriteTimeUtc(Paths.Config);
                if (!force && stamp == configStamp) return;
                List<Group> fresh = Config.Load();
                configStamp = File.GetLastWriteTimeUtc(Paths.Config);
                RestorePriorities();          // undo old settings before applying new ones
                foreach (Group g in groups) if (g.Job != IntPtr.Zero) Native.CloseHandle(g.Job);
                groups = fresh;
                foreach (Group g in groups)
                {
                    if (g.JobMemoryMB > 0 || g.ProcessMemoryMB > 0)
                    {
                        try { g.Job = Native.CreateCappedJob("Local\\MemCap-" + g.Name, g.ProcessMemoryMB, g.JobMemoryMB); }
                        catch (Exception e) { Log.Write("[" + g.Name + "] could not create job: " + e.Message); }
                    }
                }
                Log.Write("config loaded: " + groups.Count + " groups, every " + Config.IntervalSec + " s");
            }
            catch (Exception e) { Log.Write("config error: " + e.Message); }
        }

        static List<Process> Find(Group g)
        {
            List<Process> list = new List<Process>();
            foreach (string n in g.Processes)
                try { list.AddRange(Process.GetProcessesByName(n)); } catch { }
            return list;
        }

        public void Tick()
        {
            ReloadIfChanged(false);
            if (Paused) return;
            foreach (Group g in groups)
            {
                List<Process> procs = Find(g);
                long ram = 0, priv = 0;
                foreach (Process p in procs)
                {
                    try { ram += p.WorkingSet64; priv += p.PrivateMemorySize64; } catch { }
                    if (g.Priority < 5) Native.SetMemoryPriority(p.Id, (uint)g.Priority);
                    if (g.Job != IntPtr.Zero) Native.Assign(g.Job, p.Id);
                }
                long ramMB = ram / 1048576, privMB = priv / 1048576;
                if (g.TrimAboveMB > 0 && ramMB > g.TrimAboveMB)
                {
                    foreach (Process p in procs) Native.Trim(p.Id);
                    Log.Write("[" + g.Name + "] RAM " + ramMB + " MB > " + g.TrimAboveMB + " MB, trimmed " + procs.Count + " processes");
                }
                g.LastStatus = g.Name + ": " + procs.Count + " procs, " + ramMB + " MB RAM, " + privMB + " MB committed";
                g.LastRamMB = ramMB;
                foreach (Process p in procs) p.Dispose();
            }
        }

        public void RestorePriorities()
        {
            foreach (Group g in groups)
            {
                if (g.Priority >= 5) continue;
                List<Process> procs = Find(g);
                foreach (Process p in procs) { Native.SetMemoryPriority(p.Id, 5); p.Dispose(); }
            }
        }

        public string ShortSummary()
        {
            List<string> parts = new List<string>();
            foreach (Group g in groups) parts.Add(g.Name + " " + (g.LastRamMB / 1024.0).ToString("0.0") + "G");
            return string.Join(" ", parts.ToArray());
        }

        public string Summary()
        {
            StringBuilder sb = new StringBuilder();
            foreach (Group g in groups) sb.AppendLine(g.LastStatus.Length > 0 ? g.LastStatus : g.Name + ": waiting for first check");
            return sb.ToString();
        }
    }

    class TrayApp : ApplicationContext
    {
        readonly Engine engine = new Engine();
        readonly NotifyIcon tray = new NotifyIcon();
        readonly System.Windows.Forms.Timer timer = new System.Windows.Forms.Timer();
        readonly ToolStripMenuItem pauseItem;
        readonly EventWaitHandle exitSignal;
        readonly RegisteredWaitHandle exitWait;
        readonly SynchronizationContext ui;

        public TrayApp(EventWaitHandle exitSignal)
        {
            this.exitSignal = exitSignal;
            ui = new WindowsFormsSynchronizationContext();
            ContextMenuStrip menu = new ContextMenuStrip();
            menu.Items.Add("Status", null, delegate { ShowStatus(); });
            pauseItem = new ToolStripMenuItem("Pause", null, delegate { TogglePause(); });
            menu.Items.Add(pauseItem);
            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add("Edit settings", null, delegate { Open(Paths.Config); });
            menu.Items.Add("Open log", null, delegate { Open(Paths.Log); });
            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add("Exit", null, delegate { Quit(); });
            tray.ContextMenuStrip = menu;
            tray.Icon = MakeIcon();
            tray.Text = "MemCap";
            tray.Visible = true;
            tray.DoubleClick += delegate { ShowStatus(); };

            engine.ReloadIfChanged(true);
            Log.Write("MemCap started (" + Application.ExecutablePath + ")");
            timer.Tick += delegate { RunTick(); };
            RunTick();
            timer.Start();

            // Lets "MemCap --install/--uninstall" stop a running copy.
            exitWait = ThreadPool.RegisterWaitForSingleObject(exitSignal, delegate { ui.Post(delegate { Quit(); }, null); }, null, -1, true);
        }

        void RunTick()
        {
            try { engine.Tick(); } catch (Exception e) { Log.Write("tick error: " + e.Message); }
            timer.Interval = Config.IntervalSec * 1000;
            // .NET Framework NotifyIcon.Text throws above 63 characters.
            string t = engine.Paused ? "MemCap (paused)" : "MemCap " + engine.ShortSummary();
            tray.Text = t.Length > 63 ? t.Substring(0, 63) : t;
        }

        void ShowStatus()
        {
            string s = (engine.Paused ? "PAUSED\n\n" : "") + engine.Summary() + "\nSettings: " + Paths.Config + "\nLog: " + Paths.Log;
            MessageBox.Show(s, "MemCap", MessageBoxButtons.OK, MessageBoxIcon.Information);
        }

        void TogglePause()
        {
            engine.Paused = !engine.Paused;
            if (engine.Paused) engine.RestorePriorities();
            pauseItem.Text = engine.Paused ? "Resume" : "Pause";
            Log.Write(engine.Paused ? "paused" : "resumed");
            RunTick();
        }

        static void Open(string path)
        {
            try
            {
                if (!File.Exists(path)) File.WriteAllText(path, path == Paths.Config ? Config.Default : "");
                Process.Start("notepad.exe", "\"" + path + "\"");
            }
            catch { }
        }

        void Quit()
        {
            timer.Stop();
            try { engine.RestorePriorities(); } catch { }
            exitWait.Unregister(null);
            tray.Visible = false;
            tray.Dispose();
            Log.Write("MemCap stopped, memory priorities restored");
            ExitThread();
        }

        static Icon MakeIcon()
        {
            Bitmap bmp = new Bitmap(32, 32);
            using (Graphics g = Graphics.FromImage(bmp))
            {
                g.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
                g.Clear(Color.Transparent);
                using (Brush b = new SolidBrush(Color.FromArgb(40, 120, 220))) g.FillRectangle(b, 4, 6, 24, 20);
                using (Brush w = new SolidBrush(Color.White))
                {
                    g.FillRectangle(w, 8, 10, 4, 12);
                    g.FillRectangle(w, 14, 14, 4, 8);
                    g.FillRectangle(w, 20, 18, 4, 4);
                }
                using (Pen p = new Pen(Color.FromArgb(40, 120, 220), 2)) { for (int x = 7; x <= 25; x += 6) { g.DrawLine(p, x, 2, x, 6); g.DrawLine(p, x, 26, x, 30); } }
            }
            return Icon.FromHandle(bmp.GetHicon());
        }
    }

    static class Program
    {
        [STAThread]
        static int Main(string[] args)
        {
            string cmd = args.Length > 0 ? args[0].ToLowerInvariant().TrimStart('-', '/') : "";
            switch (cmd)
            {
                case "install": return Install(args.Length > 1 && args[1].ToLowerInvariant().Contains("quiet"));
                case "uninstall": return Uninstall(args.Length > 1 && args[1].ToLowerInvariant().Contains("quiet"));
                case "stop": SignalExit(); return 0;
                case "": break;
                default:
                    MessageBox.Show("MemCap [install | uninstall | stop]\n\nNo argument: run in the tray.\ninstall: copy to " + Paths.InstallDir + ", start at logon, start now.\nuninstall: stop, remove from logon and delete the program (settings and log are kept in " + Paths.DataDir + ").", "MemCap");
                    return 1;
            }

            using (Mutex m = new Mutex(false, Paths.Mutex))
            {
                // Wait a little: during install/restart the previous copy may still be shutting down.
                bool owned;
                try { owned = m.WaitOne(8000); }
                catch (AbandonedMutexException) { owned = true; }
                if (!owned) return 0;   // another copy is running
                using (EventWaitHandle exit = new EventWaitHandle(false, EventResetMode.AutoReset, Paths.ExitEvent))
                {
                    Application.SetUnhandledExceptionMode(UnhandledExceptionMode.CatchException);
                    Application.ThreadException += delegate(object s, ThreadExceptionEventArgs e) { Log.Write("error: " + e.Exception); };
                    AppDomain.CurrentDomain.UnhandledException += delegate(object s, UnhandledExceptionEventArgs e) { Log.Write("fatal: " + e.ExceptionObject); };
                    Application.EnableVisualStyles();
                    Application.Run(new TrayApp(exit));
                }
            }
            return 0;
        }

        static bool IsRunning()
        {
            Mutex probe;
            if (Mutex.TryOpenExisting(Paths.Mutex, out probe)) { probe.Dispose(); return true; }
            return false;
        }

        static void SignalExit()
        {
            EventWaitHandle e;
            if (!EventWaitHandle.TryOpenExisting(Paths.ExitEvent, out e)) return;
            using (e) e.Set();
            for (int i = 0; i < 50 && IsRunning(); i++) Thread.Sleep(100);
        }

        static int Install(bool quiet)
        {
            try
            {
                SignalExit();
                Directory.CreateDirectory(Paths.InstallDir);
                string target = Path.Combine(Paths.InstallDir, "MemCap.exe");
                string self = Application.ExecutablePath;
                if (!string.Equals(Path.GetFullPath(self), Path.GetFullPath(target), StringComparison.OrdinalIgnoreCase))
                {
                    for (int i = 0; ; i++)
                    {
                        try { File.Copy(self, target, true); break; }
                        catch (IOException) { if (i > 20) throw; Thread.Sleep(250); }
                    }
                }
                Config.Load();   // writes default settings if there are none
                using (RegistryKey k = Registry.CurrentUser.CreateSubKey(Paths.RunKey)) k.SetValue("MemCap", "\"" + target + "\"");
                Process.Start(target);
                Log.Write("installed to " + target);
                if (!quiet) MessageBox.Show("MemCap is installed and running in the tray.\nIt starts automatically when you sign in.\n\nProgram: " + target + "\nSettings: " + Paths.Config, "MemCap");
                return 0;
            }
            catch (Exception e)
            {
                if (!quiet) MessageBox.Show("Install failed: " + e.Message, "MemCap", MessageBoxButtons.OK, MessageBoxIcon.Error);
                return 2;
            }
        }

        static int Uninstall(bool quiet)
        {
            try
            {
                SignalExit();
                using (RegistryKey k = Registry.CurrentUser.OpenSubKey(Paths.RunKey, true)) { if (k != null) k.DeleteValue("MemCap", false); }
                string target = Path.Combine(Paths.InstallDir, "MemCap.exe");
                bool runningFromInstall = string.Equals(Path.GetFullPath(Application.ExecutablePath), Path.GetFullPath(target), StringComparison.OrdinalIgnoreCase);
                if (runningFromInstall)
                {
                    // Can't delete our own exe while running: let cmd do it after we exit.
                    ProcessStartInfo psi = new ProcessStartInfo("cmd.exe", "/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"" + Paths.InstallDir + "\"");
                    psi.CreateNoWindow = true; psi.UseShellExecute = false;
                    Process.Start(psi);
                }
                else if (Directory.Exists(Paths.InstallDir)) Directory.Delete(Paths.InstallDir, true);
                Log.Write("uninstalled");
                if (!quiet) MessageBox.Show("MemCap is removed and no longer starts at sign-in.\nSettings and log were kept in " + Paths.DataDir + ".", "MemCap");
                return 0;
            }
            catch (Exception e)
            {
                if (!quiet) MessageBox.Show("Uninstall failed: " + e.Message, "MemCap", MessageBoxButtons.OK, MessageBoxIcon.Error);
                return 2;
            }
        }
    }
}
