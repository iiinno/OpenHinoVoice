// Performance probe for a dedicated desktop. No SwitchDesktop, global input or recording.
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using System.Web.Script.Serialization;
using System.Windows.Automation;

internal static class PerformanceProbe
{
    public sealed class Artifact
    {
        public string label, path, sha256, revision, features, uiaExpected, hudStyle;
        public long bytes;
    }
    public sealed class Plan
    {
        public string mode, namespaceValue, output;
        public int repetitions, durationSeconds, intervalMs, warmupMs, timeoutMs;
        public Artifact[] artifacts;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct StartupInfo
    {
        public int cb;
        public string reserved, desktop, title;
        public uint x, y, width, height, charsX, charsY, fill, flags;
        public short show, reservedBytes;
        public IntPtr reservedData, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo { public IntPtr process, thread; public uint processId, threadId; }
    [StructLayout(LayoutKind.Sequential)]
    struct Rect { public int left, top, right, bottom; }
    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimit
    {
        public long processTime, jobTime;
        public uint flags;
        public UIntPtr minWorkingSet, maxWorkingSet;
        public uint activeProcessLimit;
        public UIntPtr affinity;
        public uint priorityClass, schedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IoCounters { public ulong readOps, writeOps, otherOps, readBytes, writeBytes, otherBytes; }
    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimit
    {
        public BasicLimit basic; public IoCounters io;
        public UIntPtr processMemory, jobMemory, peakProcessMemory, peakJobMemory;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct Accounting
    {
        public long user, kernel, periodUser, periodKernel;
        public uint pageFaults, totalProcesses, activeProcesses, terminatedProcesses;
    }
    delegate bool EnumWindow(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr mode, uint flags, uint access, IntPtr security);
    [DllImport("user32.dll", SetLastError = true)] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr GetProcessWindowStation();
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool GetUserObjectInformationW(IntPtr handle, int index, StringBuilder output, int length, out int needed);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder command, IntPtr processAttributes, IntPtr threadAttributes,
        bool inherit, uint flags, IntPtr environment, string directory, ref StartupInfo startup, out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern uint SetErrorMode(uint mode);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr OpenJobObjectW(uint access, bool inherit, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int type, ref ExtendedLimit value, uint size);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool QueryInformationJobObject(IntPtr job, int type, out Accounting value, uint length, IntPtr returnedLength);
    [DllImport("kernel32.dll", EntryPoint = "QueryInformationJobObject", SetLastError = true)]
    static extern bool QueryJobProcessIds(IntPtr job, int type, IntPtr value, uint length, IntPtr returnedLength);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool member);
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint processId);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder path, ref uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out long created, out long exited, out long kernel, out long user);
    [DllImport("kernel32.dll")] static extern IntPtr GetConsoleWindow();
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindow callback, IntPtr parameter);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr window, StringBuilder name, int count);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr window, StringBuilder text, int count);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr window);
    [DllImport("user32.dll", SetLastError = true)]
    static extern bool AdjustWindowRectExForDpi(ref Rect rect, uint style, bool menu, uint extended, uint dpi);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongW")] static extern int GetWindowLong(IntPtr window, int index);
    [DllImport("user32.dll", SetLastError = true)]
    static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll", SetLastError = true)] static extern bool PostMessageW(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll", SetLastError = true)]
    static extern IntPtr SendMessageTimeoutW(IntPtr window, uint message, IntPtr wparam, IntPtr lparam, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll")] static extern uint GetGuiResources(IntPtr process, uint flags);
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr context);

    static readonly JavaScriptSerializer Json = new JavaScriptSerializer { MaxJsonLength = 32 * 1024 * 1024 };
    static Plan Settings;
    static Dictionary<string, object> Report;
    static Dictionary<string, object> Map(params object[] pairs)
    {
        var result = new Dictionary<string, object>();
        for (int i = 0; i < pairs.Length; i += 2) result[(string)pairs[i]] = pairs[i + 1];
        return result;
    }
    static void Save(string path, object value)
    {
        File.WriteAllText(path, Json.Serialize(value), new UTF8Encoding(false));
    }
    static void Phase(string phase)
    {
        Report["phase"] = phase;
        Report["updatedAt"] = DateTime.UtcNow.ToString("o");
        Save(Path.Combine(Settings.output, "worker-result.json"), Report);
    }
    static void Require(bool condition, string operation)
    {
        if (!condition) throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
    }
    static string Quote(string value)
    {
        var result = new StringBuilder("\"");
        int slashes = 0;
        foreach (char character in value)
        {
            if (character == '\\') { slashes++; continue; }
            if (character == '"') result.Append('\\', slashes * 2 + 1).Append('"');
            else result.Append('\\', slashes).Append(character);
            slashes = 0;
        }
        return result.Append('\\', slashes * 2).Append('"').ToString();
    }
    static string InputDesktop()
    {
        IntPtr desktop = OpenInputDesktop(0, false, 1);
        if (desktop == IntPtr.Zero) return "unavailable";
        try
        {
            int required;
            var name = new StringBuilder(256);
            return GetUserObjectInformationW(desktop, 2, name, name.Capacity * 2, out required)
                ? name.ToString() : "unavailable";
        }
        finally { CloseDesktop(desktop); }
    }
    [MTAThread]
    static int Main(string[] args)
    {
        if (args.Length != 2 || (args[0] != "--controller" && args[0] != "--worker")) return 2;
        if (Environment.GetEnvironmentVariable("GITHUB_ACTIONS") != "true")
        {
            Console.Error.WriteLine("This measurement helper runs only in GitHub Actions.");
            return 1;
        }
        SetErrorMode(0x0001 | 0x0002 | 0x8000);
        try
        {
            Settings = Json.Deserialize<Plan>(File.ReadAllText(args[1]));
            if ((Settings.mode != "Resident" && Settings.mode != "Settings") || Settings.artifacts == null || Settings.artifacts.Length != 2
                || Settings.repetitions < 3 || Settings.repetitions > 10 || Settings.durationSeconds < 30
                || Settings.durationSeconds > 60 || Settings.intervalMs < 100 || Settings.intervalMs > 5000)
                throw new InvalidOperationException("Invalid bounded measurement plan.");
            string longestNamespace = Settings.namespaceValue + "-r10-candidate";
            if (!System.Text.RegularExpressions.Regex.IsMatch(longestNamespace, "^[A-Za-z0-9_-]{1,64}$")
                || Settings.warmupMs < 1000 || Settings.warmupMs > 10000
                || String.IsNullOrEmpty(Settings.output) || !Path.IsPathRooted(Settings.output)
                || Settings.timeoutMs < 1000 || Settings.timeoutMs > 2400000
                || Settings.artifacts.Select(a => a.label).Distinct().Count() != 2)
                throw new InvalidOperationException("Invalid isolation or deadline parameters.");
            foreach (var artifact in Settings.artifacts)
            {
                if ((artifact.label != "baseline" && artifact.label != "candidate")
                    || !new[] { "Absent", "Present", "Either" }.Contains(artifact.uiaExpected)
                    || !new[] { "standard", "soft", "minimal", "rich" }.Contains(artifact.hudStyle)
                    || String.IsNullOrEmpty(artifact.path) || !Path.IsPathRooted(artifact.path) || !File.Exists(artifact.path))
                    throw new InvalidOperationException("Invalid artifact parameters.");
            }
            return args[0] == "--controller" ? Controller(Path.GetFullPath(args[1])) : Worker();
        }
        catch (Exception error) { Console.Error.WriteLine(error); return 1; }
    }
    static int Controller(string planPath)
    {
        var result = Map("status", "running", "startedAt", DateTime.UtcNow.ToString("o"),
            "inputDesktopBefore", InputDesktop(), "desktopSwitched", false, "workerExitCode", null);
        IntPtr desktop = IntPtr.Zero, job = IntPtr.Zero;
        ProcessInfo child = new ProcessInfo();
        int code = 1;
        try
        {
            // DESKTOP_SWITCHDESKTOP is deliberately excluded.
            string name = "OHV_" + Settings.namespaceValue;
            desktop = CreateDesktopW(name, IntPtr.Zero, IntPtr.Zero, 0, 0x00FF, IntPtr.Zero);
            Require(desktop != IntPtr.Zero, "CreateDesktopW");
            result["desktop"] = name;
            job = CreateJobObjectW(IntPtr.Zero, JobName());
            Require(job != IntPtr.Zero, "CreateJobObjectW");
            if (Marshal.GetLastWin32Error() == 183) throw new InvalidOperationException("Measurement job already exists.");
            result["jobName"] = JobName();
            var limit = new ExtendedLimit(); limit.basic.flags = 0x2000;
            Require(SetInformationJobObject(job, 9, ref limit, (uint)Marshal.SizeOf(typeof(ExtendedLimit))), "SetInformationJobObject");
            string exe = Process.GetCurrentProcess().MainModule.FileName;
            IntPtr station = GetProcessWindowStation();
            Require(station != IntPtr.Zero, "GetProcessWindowStation");
            int stationLength; var stationName = new StringBuilder(256);
            Require(GetUserObjectInformationW(station, 2, stationName, stationName.Capacity * 2, out stationLength), "Window station name");
            result["windowStation"] = stationName.ToString();
            var startup = new StartupInfo { cb = Marshal.SizeOf(typeof(StartupInfo)), desktop = stationName.ToString() + @"\" + name };
            Require(CreateProcessW(exe, new StringBuilder(Quote(exe) + " --worker " + Quote(planPath)),
                IntPtr.Zero, IntPtr.Zero, false, 0x08000004, IntPtr.Zero, Settings.output, ref startup, out child), "CreateProcessW");
            result["workerPid"] = child.processId;
            Require(AssignProcessToJobObject(job, child.process), "AssignProcessToJobObject");
            Require(ResumeThread(child.thread) != UInt32.MaxValue, "ResumeThread");
            CloseHandle(child.thread); child.thread = IntPtr.Zero;
            Save(Path.Combine(Settings.output, "controller-result.json"), result);
            uint waited = WaitForSingleObject(child.process, (uint)Settings.timeoutMs);
            if (waited == 258) throw new TimeoutException("Measurement deadline exceeded; only this job will be closed.");
            Require(waited == 0, "WaitForSingleObject");
            uint workerCode;
            Require(GetExitCodeProcess(child.process, out workerCode), "GetExitCodeProcess");
            result["workerExitCode"] = workerCode;
            JobState finalJob = null;
            bool empty = Until(delegate
            {
                finalJob = ReadJobState(job);
                return finalJob.error == null && finalJob.processes.Count == 0;
            }, 5000);
            result["finalJobState"] = finalJob;
            result["residualChildProcesses"] = finalJob.error == null ? (object)finalJob.processes.Count : null;
            if (workerCode != 0 || !empty)
                throw new InvalidOperationException("Worker failed or left child processes; inspect worker-result.json.");
            result["status"] = "measured";
            code = 0;
        }
        catch (Exception error) { result["status"] = "failed"; result["error"] = error.ToString(); }
        finally
        {
            if (job != IntPtr.Zero)
            {
                bool closed = CloseHandle(job); result["jobClosed"] = closed;
                if (!closed) { result["status"] = "failed"; code = 1; }
            }
            if (child.process != IntPtr.Zero)
            {
                if (WaitForSingleObject(child.process, 2000) == 258) TerminateProcess(child.process, 1);
                WaitForSingleObject(child.process, 5000); CloseHandle(child.process);
            }
            if (child.thread != IntPtr.Zero) CloseHandle(child.thread);
            if (desktop != IntPtr.Zero)
            {
                bool closed = CloseDesktop(desktop);
                result["desktopClosed"] = closed;
                if (!closed) { result["status"] = "failed"; code = 1; }
            }
            string after = InputDesktop(); result["inputDesktopAfter"] = after;
            if (after == "unavailable" || !Equals(result["inputDesktopBefore"], after))
            {
                result["status"] = "failed"; result["desktopObservation"] = "Input desktop could not be confirmed unchanged."; code = 1;
            }
            result["finishedAt"] = DateTime.UtcNow.ToString("o");
            Save(Path.Combine(Settings.output, "controller-result.json"), result);
        }
        return code;
    }
    static bool Until(Func<bool> condition, int milliseconds)
    {
        var watch = Stopwatch.StartNew();
        using (var wait = new ManualResetEvent(false))
        {
            do { if (condition()) return true; wait.WaitOne(50); } while (watch.ElapsedMilliseconds < milliseconds);
        }
        return false;
    }
    static IntPtr FindWindow(int pid, bool settings)
    {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr window, IntPtr ignored)
        {
            uint owner; GetWindowThreadProcessId(window, out owner);
            if (owner != (uint)pid) return true;
            var className = new StringBuilder(256); GetClassNameW(window, className, className.Capacity);
            var title = new StringBuilder(256); GetWindowTextW(window, title, title.Capacity);
            Rect rect;
            bool matches = settings
                ? className.ToString() == "Window Class" && title.ToString() == "OpenHinoVoice"
                    && IsWindowVisible(window) && GetClientRect(window, out rect) && rect.right > 100 && rect.bottom > 100
                : className.ToString() == "OpenHinoVoiceOverlay";
            if (matches) { found = window; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    static bool Responds(IntPtr window)
    {
        UIntPtr ignored;
        return SendMessageTimeoutW(window, 0, IntPtr.Zero, IntPtr.Zero, 0x23, 500, out ignored) != IntPtr.Zero;
    }
    static Dictionary<string, object> Metrics(Process process, double elapsed)
    {
        process.Refresh();
        if (process.HasExited) throw new InvalidOperationException("Measured process exited.");
        return Map("elapsedMs", elapsed, "privateBytes", process.PrivateMemorySize64,
            "workingSetBytes", process.WorkingSet64, "cpuMs", process.TotalProcessorTime.TotalMilliseconds,
            "gdi", GetGuiResources(process.Handle, 0), "user", GetGuiResources(process.Handle, 1), "handles", process.HandleCount);
    }
    static List<Dictionary<string, object>> Observe(Process process, int durationMs, List<Dictionary<string, object>> rows = null)
    {
        if (rows == null) rows = new List<Dictionary<string, object>>();
        var watch = Stopwatch.StartNew();
        using (var wait = new ManualResetEvent(false))
        {
            do
            {
                rows.Add(Metrics(process, watch.Elapsed.TotalMilliseconds));
                int remaining = durationMs - (int)watch.ElapsedMilliseconds;
                if (remaining <= 0) break;
                // This is an explicit sampling interval, never an inference of completion/readiness.
                wait.WaitOne(Math.Min(Settings.intervalMs, remaining));
            } while (true);
        }
        return rows;
    }
    static Dictionary<string, object> Summary(List<Dictionary<string, object>> rows)
    {
        var last = rows[rows.Count - 1]; var first = rows[0];
        return Map("sampleCount", rows.Count, "elapsedMs", last["elapsedMs"],
            "cpuMs", Convert.ToDouble(last["cpuMs"]) - Convert.ToDouble(first["cpuMs"]),
            "peakPrivateBytes", rows.Max(r => Convert.ToInt64(r["privateBytes"])),
            "peakWorkingSetBytes", rows.Max(r => Convert.ToInt64(r["workingSetBytes"])),
            "startGdi", first["gdi"], "endGdi", last["gdi"], "peakGdi", rows.Max(r => Convert.ToInt64(r["gdi"])),
            "startUser", first["user"], "endUser", last["user"], "peakUser", rows.Max(r => Convert.ToInt64(r["user"])));
    }
    static Dictionary<string, object> ProbeUia(IntPtr window, int processId, string expected)
    {
        const int observationMs = 5000;
        var watch = Stopwatch.StartNew();
        int count = 0, attempts = 0, reads = 0;
        bool named = false, completeWindow = false, faulted = false;
        string lastError = null;
        using (var wait = new ManualResetEvent(false))
        {
            while (true)
            {
                attempts++;
                uint owner;
                if (GetWindowThreadProcessId(window, out owner) == 0 || owner != (uint)processId || !Responds(window))
                {
                    faulted = true; lastError = "Measured window was lost, replaced or stopped responding.";
                    break;
                }
                try
                {
                    var elements = AutomationElement.FromHandle(window).FindAll(TreeScope.Subtree, Condition.TrueCondition);
                    count = elements.Count;
                    if (count > 4096) throw new InvalidOperationException("UIA element bound exceeded.");
                    for (int i = 0; i < count; i++) if (elements[i].Current.Name == "試す") named = true;
                    if (GetWindowThreadProcessId(window, out owner) == 0 || owner != (uint)processId)
                        throw new InvalidOperationException("Measured window was lost during the UIA query.");
                    reads++;
                }
                catch (Exception error)
                {
                    lastError = error.Message;
                    // An unavailable provider is not evidence that accessibility is disabled.
                    if (expected == "Absent") { faulted = true; break; }
                }
                if (expected == "Absent" && named)
                {
                    faulted = true; lastError = "Expected feature-off controls appeared during the observation window.";
                    break;
                }
                if (watch.ElapsedMilliseconds >= observationMs)
                {
                    completeWindow = true;
                    break;
                }
                wait.WaitOne((int)Math.Min(100, Math.Max(0, observationMs - watch.ElapsedMilliseconds)));
            }
        }
        bool matches = !faulted && completeWindow && reads > 0 && (expected == "Absent"
            ? completeWindow && !named && lastError == null
            : expected == "Either" || named);
        return Map("status", matches ? (expected == "Absent" ? "expected_absence" : "observed") : "failed",
            "expected", expected, "namedDisplayControl", named, "nodes", count, "attempts", attempts, "successfulReads", reads,
            "requiredObservationMs", observationMs, "fullObservationWindow", completeWindow,
            "durationMs", watch.Elapsed.TotalMilliseconds, "error", lastError);
    }
    static string JobName() { return @"Local\OpenHinoVoice.Perf." + Settings.namespaceValue; }
    public sealed class JobProcess
    {
        public uint pid;
        public string imagePath, error;
        public long creationTime;
        public bool ownedJob;
    }
    public sealed class JobState
    {
        public string observedAt = DateTime.UtcNow.ToString("o"), error;
        public uint accountingActiveProcesses, assignedProcesses;
        public int accountingStructureBytes = Marshal.SizeOf(typeof(Accounting));
        public List<JobProcess> processes = new List<JobProcess>();
    }
    static JobProcess ReadJobProcess(uint pid, IntPtr job)
    {
        var identity = new JobProcess { pid = pid };
        IntPtr process = OpenProcess(0x1000, false, pid); // QUERY_LIMITED_INFORMATION only.
        try
        {
            Require(process != IntPtr.Zero, "OpenProcess for job identity");
            Require(IsProcessInJob(process, job, out identity.ownedJob), "IsProcessInJob");
            if (!identity.ownedJob) throw new InvalidOperationException("PID no longer belongs to this job.");
            uint length = 32768; var path = new StringBuilder((int)length);
            Require(QueryFullProcessImageNameW(process, 0, path, ref length), "QueryFullProcessImageNameW");
            identity.imagePath = path.ToString();
            long exited, kernel, user;
            Require(GetProcessTimes(process, out identity.creationTime, out exited, out kernel, out user), "GetProcessTimes");
            uint code;
            Require(GetExitCodeProcess(process, out code), "GetExitCodeProcess for job identity");
            if (code != 259) throw new InvalidOperationException("Process exited during the job snapshot.");
        }
        catch (Exception error) { identity.error = error.Message; }
        finally { if (process != IntPtr.Zero) CloseHandle(process); }
        return identity;
    }
    static JobState ReadJobState(IntPtr job)
    {
        var state = new JobState();
        try
        {
            // Four LARGE_INTEGERs followed by four DWORDs; ActiveProcesses is at byte 40.
            if (state.accountingStructureBytes != 48 || Marshal.OffsetOf(typeof(Accounting), "activeProcesses").ToInt32() != 40)
                throw new InvalidOperationException("Unexpected JOBOBJECT_BASIC_ACCOUNTING_INFORMATION layout.");
            Accounting accounting;
            Require(QueryInformationJobObject(job, 1, out accounting, 48, IntPtr.Zero), "Query job accounting");
            state.accountingActiveProcesses = accounting.activeProcesses;
            // JOBOBJECT_BASIC_PROCESS_ID_LIST: two DWORDs, then pointer-sized PIDs.
            for (int capacity = 16; capacity <= 1024; capacity *= 2)
            {
                int bytes = 8 + capacity * IntPtr.Size;
                IntPtr buffer = Marshal.AllocHGlobal(bytes);
                try
                {
                    bool read = QueryJobProcessIds(job, 3, buffer, (uint)bytes, IntPtr.Zero);
                    int error = Marshal.GetLastWin32Error();
                    if (!read)
                    {
                        if (error == 234) continue; // ERROR_MORE_DATA, never accept a partial list.
                        throw new Win32Exception(error, "Query job process IDs");
                    }
                    uint assigned = unchecked((uint)Marshal.ReadInt32(buffer, 0));
                    uint listed = unchecked((uint)Marshal.ReadInt32(buffer, 4));
                    if (listed < assigned || listed > capacity) continue;
                    state.assignedProcesses = assigned;
                    for (int i = 0; i < listed; i++)
                    {
                        long pid = Marshal.ReadIntPtr(buffer, 8 + i * IntPtr.Size).ToInt64();
                        if (pid <= 0 || pid > UInt32.MaxValue) throw new InvalidOperationException("Invalid job PID.");
                        state.processes.Add(ReadJobProcess((uint)pid, job));
                    }
                    return state;
                }
                finally { Marshal.FreeHGlobal(buffer); }
            }
            throw new InvalidOperationException("Job process-list bound exceeded.");
        }
        catch (Exception error) { state.error = error.Message; }
        return state;
    }
    static bool SameProcess(JobProcess left, JobProcess right)
    {
        return left.error == null && right.error == null && left.pid == right.pid
            && left.creationTime == right.creationTime
            && String.Equals(left.imagePath, right.imagePath, StringComparison.OrdinalIgnoreCase);
    }
    static List<JobProcess> CaptureJobBaseline(IntPtr job, JobProcess worker)
    {
        JobState first = null, last = null;
        string consoleImage = Path.Combine(Environment.SystemDirectory, "conhost.exe");
        int attempts = 0;
        bool valid = Until(delegate
        {
            last = ReadJobState(job); attempts++;
            if (first == null) first = last;
            return last.error == null && last.processes.Any(p => SameProcess(p, worker))
                && last.processes.All(p => SameProcess(p, worker) || (p.error == null
                    && p.creationTime >= worker.creationTime
                    && String.Equals(p.imagePath, consoleImage, StringComparison.OrdinalIgnoreCase)));
        }, 5000);
        uint consoleOwner = 0;
        IntPtr console = GetConsoleWindow();
        if (console != IntPtr.Zero) GetWindowThreadProcessId(console, out consoleOwner);
        Report["jobBaseline"] = Map("status", valid ? "confirmed" : "failed", "first", first, "last", last,
            "attempts", attempts, "workerConsoleWindowOwnerPid", consoleOwner, "allowedConsoleImage", consoleImage,
            "rule", "Before any app launch: own worker and job-member System32 conhost created no earlier than the worker. Later boundaries allow only these exact PID/creation-time/image identities; console hosts may exit.");
        Phase("job_baseline");
        if (!valid) throw new InvalidOperationException("Initial job contains an unverified process; inspect jobBaseline.");
        return last.processes;
    }
    static Dictionary<string, object> ObserveJobBoundary(IntPtr job, JobProcess worker, List<JobProcess> baseline)
    {
        JobState first = null, last = null;
        int attempts = 0;
        var watch = Stopwatch.StartNew();
        bool clean = Until(delegate
        {
            last = ReadJobState(job); attempts++;
            if (first == null) first = last;
            return last.error == null && last.processes.Any(p => SameProcess(p, worker))
                && last.processes.All(p => baseline.Any(known => SameProcess(p, known)));
        }, 5000);
        return Map("status", clean ? "confirmed" : "failed", "first", first, "last", last,
            "attempts", attempts, "deadlineMs", 5000, "durationMs", watch.Elapsed.TotalMilliseconds);
    }
    static Dictionary<string, object> Measure(Artifact artifact, int round, int order)
    {
        bool settings = Settings.mode == "Settings";
        string label = "r" + round + "-" + artifact.label;
        string directory = Path.Combine(Settings.output, label); Directory.CreateDirectory(directory);
        string namespaceValue = Settings.namespaceValue + "-" + label;
        var row = Map("artifact", artifact.label, "round", round, "order", order, "status", "running",
            "pid", null, "gracefulExit", false, "windowResponsive", false, "uiMode", Settings.mode,
            "requestedPage", settings ? "display" : null, "hudStyle", artifact.hudStyle);
        ((List<Dictionary<string, object>>)Report["cases"]).Add(row);
        Process process = null; IntPtr window = IntPtr.Zero;
        Task<string> stdout = null, stderr = null;
        var launch = Stopwatch.StartNew();
        try
        {
            string appdata = Path.Combine(directory, "appdata"), local = Path.Combine(directory, "localappdata");
            Directory.CreateDirectory(Path.Combine(appdata, "OpenHinoVoice")); Directory.CreateDirectory(local);
            string config = "speech_exe = \"missing-perf-fixture-speech.exe\"\nmodel = \"model-not-used.gguf\"\n"
                + "hotkey = \"Ctrl+Alt+Shift+F24\"\nsounds = false\nvolume = 0\nstartup_sound = false\n"
                + "recovery_history = false\nkeep_worker_minutes = 0\nhud_style = \"" + artifact.hudStyle
                + "\"\n[format]\noffline = false\nusage_log = false\nsave_history = false\n";
            File.WriteAllText(Path.Combine(appdata, "OpenHinoVoice", "config.toml"), config, new UTF8Encoding(false));
            var start = new ProcessStartInfo(artifact.path, settings ? "--settings" : "")
            {
                WorkingDirectory = directory, UseShellExecute = false, CreateNoWindow = true,
                RedirectStandardOutput = true, RedirectStandardError = true
            };
            start.EnvironmentVariables["APPDATA"] = appdata;
            start.EnvironmentVariables["LOCALAPPDATA"] = local;
            start.EnvironmentVariables["OHV_TEST_NAMESPACE"] = namespaceValue;
            start.EnvironmentVariables["OHV_TEST_SETTINGS_PAGE"] = "display";
            start.EnvironmentVariables["OHV_TEST_SETTINGS_SIZE"] = "720x520";
            start.EnvironmentVariables.Remove("OHV_TEST_SETTINGS_CAPTURE_DIR");
            process = Process.Start(start);
            if (process == null) throw new InvalidOperationException("Process did not start.");
            row["pid"] = process.Id;
            stdout = process.StandardOutput.ReadToEndAsync(); stderr = process.StandardError.ReadToEndAsync();
            Phase(label + ":window");
            if (!Until(delegate
            {
                if (process.HasExited) return true;
                window = FindWindow(process.Id, settings);
                return window != IntPtr.Zero && Responds(window);
            }, 15000) || process.HasExited || window == IntPtr.Zero)
                throw new InvalidOperationException("Own responsive main window was not observed. A resident may require Explorer's tray on this desktop.");
            row["windowResponsive"] = true;
            row["startupToResponsiveWindowMs"] = launch.Elapsed.TotalMilliseconds;
            if (settings)
            {
                uint dpi = GetDpiForWindow(window); Require(dpi != 0, "GetDpiForWindow");
                int width = (int)Math.Round(720 * dpi / 96.0), height = (int)Math.Round(520 * dpi / 96.0);
                var rect = new Rect { right = width, bottom = height };
                Require(AdjustWindowRectExForDpi(ref rect, (uint)GetWindowLong(window, -16), false,
                    (uint)GetWindowLong(window, -20), dpi), "AdjustWindowRectExForDpi");
                Require(SetWindowPos(window, IntPtr.Zero, 0, 0, rect.right - rect.left, rect.bottom - rect.top, 0x16), "SetWindowPos");
                if (!Until(delegate { Rect actual; return GetClientRect(window, out actual) && actual.right == width && actual.bottom == height; }, 5000))
                    throw new InvalidOperationException("Settings did not reach the requested client size.");
                row["dpi"] = dpi; row["clientPixels"] = new[] { width, height };
            }
            Phase(label + ":warmup");
            row["warmup"] = Summary(Observe(process, Settings.warmupMs));
            Phase(label + ":sampling");
            int beforeMs = settings ? Settings.durationSeconds * 500 : Settings.durationSeconds * 1000;
            var before = new List<Dictionary<string, object>>(); row["beforeSamples"] = before;
            Observe(process, beforeMs, before); row["beforeUia"] = Summary(before);
            if (settings)
            {
                Phase(label + ":uia");
                var uia = ProbeUia(window, process.Id, artifact.uiaExpected); row["uia"] = uia;
                if ((string)uia["status"] == "failed") throw new InvalidOperationException("UIA observation did not match the requested feature condition.");
                Phase(label + ":after_uia");
                var after = new List<Dictionary<string, object>>(); row["afterSamples"] = after;
                Observe(process, Settings.durationSeconds * 1000 - beforeMs, after); row["afterUia"] = Summary(after);
            }
            if (!Responds(window)) throw new InvalidOperationException("Own window stopped responding during observation.");
            row["status"] = "measured";
        }
        catch (Exception error) { row["status"] = "failed"; row["error"] = error.ToString(); }
        finally
        {
            Phase(label + ":close");
            if (process != null)
            {
                if (!process.HasExited)
                {
                    uint owner = 0;
                    if (window != IntPtr.Zero) GetWindowThreadProcessId(window, out owner);
                    var closing = Stopwatch.StartNew();
                    if (owner == (uint)process.Id) PostMessageW(window, 0x10, IntPtr.Zero, IntPtr.Zero);
                    bool closed = process.WaitForExit(7000);
                    row["gracefulExit"] = closed; row["closeMs"] = closing.Elapsed.TotalMilliseconds;
                    if (!closed)
                    {
                        row["status"] = "failed"; row["closeError"] = "WM_CLOSE deadline exceeded; exact process terminated.";
                        process.Kill(); process.WaitForExit(5000);
                    }
                }
                if (!(bool)row["gracefulExit"]) row["status"] = "failed";
                row["exitCode"] = process.HasExited ? (object)process.ExitCode : null;
                if (!process.HasExited || process.ExitCode != 0) row["status"] = "failed";
                if (stdout != null && stdout.Wait(1000)) File.WriteAllText(Path.Combine(directory, "stdout.log"), stdout.Result, new UTF8Encoding(false));
                if (stderr != null && stderr.Wait(1000)) File.WriteAllText(Path.Combine(directory, "stderr.log"), stderr.Result, new UTF8Encoding(false));
                process.Dispose();
            }
            row["finishedAt"] = DateTime.UtcNow.ToString("o");
            Save(Path.Combine(directory, "result.json"), row);
        }
        return row;
    }
    static int Worker()
    {
        SetProcessDpiAwarenessContext(new IntPtr(-4));
        var rows = new List<Dictionary<string, object>>();
        var notRun = new List<Dictionary<string, object>>();
        Report = Map("schemaVersion", 1, "status", "running", "startedAt", DateTime.UtcNow.ToString("o"),
            "mode", Settings.mode, "cases", rows, "notRun", notRun, "audioRequested", false, "microphoneRequested", false,
            "globalInputSent", false, "os", Environment.OSVersion.ToString(), "processorCount", Environment.ProcessorCount,
            "sessionId", Process.GetCurrentProcess().SessionId,
            "scope", "owned app process on an inactive desktop; helper, speech and other processes excluded",
            "limitations", new[] { "Inactive desktop idle is not interactive desktop or active HUD performance.",
                "Startup ends at an observed responsive HWND, not first visible pixels or recognition readiness.",
                "Warmup is a fixed excluded observation after responsiveness, not proof of asynchronous completion.",
                "UIA activation includes provider initialization. Expected absence requires the full bounded observation window.",
                "No keyboard, microphone, speech recognition, paste, screenshot or external API operation is invoked.",
                "CPU load, caches and power settings are not controlled; repetitions and order are recorded." });
        Phase("start");
        bool passed = true, blocked = false;
        IntPtr job = IntPtr.Zero;
        try
        {
            job = OpenJobObjectW(0x0004, false, JobName()); // QUERY only; the controller owns job lifetime.
            Require(job != IntPtr.Zero, "OpenJobObjectW");
            var worker = ReadJobProcess((uint)Process.GetCurrentProcess().Id, job);
            if (worker.error != null) throw new InvalidOperationException(worker.error);
            var baseline = CaptureJobBaseline(job, worker);
            for (int round = 1; round <= Settings.repetitions; round++)
            {
                var order = round % 2 == 1 ? Settings.artifacts : Settings.artifacts.Reverse().ToArray();
                for (int i = 0; i < order.Length; i++)
                {
                    if (blocked)
                    {
                        notRun.Add(Map("artifact", order[i].label, "round", round, "order", i + 1,
                            "status", "not_run_after_cleanup_failure"));
                        continue;
                    }
                    var before = ObserveJobBoundary(job, worker, baseline);
                    Report["lastCaseBoundary"] = before;
                    if ((string)before["status"] != "confirmed")
                    {
                        passed = false; blocked = true;
                        Report["cleanupError"] = "Job identities did not return to the verified infrastructure baseline.";
                        notRun.Add(Map("artifact", order[i].label, "round", round, "order", i + 1,
                            "status", "not_run_due_to_residual_processes", "jobBoundary", before));
                        Phase("cleanup_failed");
                        continue;
                    }
                    var row = Measure(order[i], round, i + 1);
                    row["caseStartBoundary"] = before;
                    var after = ObserveJobBoundary(job, worker, baseline);
                    row["caseCleanup"] = after;
                    if ((string)after["status"] != "confirmed")
                    {
                        row["status"] = "failed";
                        row["cleanupError"] = "A process outside the verified infrastructure baseline remained; later measurements were not started.";
                        blocked = true;
                    }
                    Save(Path.Combine(Settings.output, "r" + round + "-" + order[i].label, "result.json"), row);
                    passed &= (string)row["status"] == "measured";
                    Phase("case_complete");
                }
            }
        }
        catch (Exception error)
        {
            passed = false; Report["error"] = error.ToString();
            for (int round = 1; round <= Settings.repetitions; round++)
            {
                foreach (var artifact in Settings.artifacts)
                {
                    if (!rows.Any(r => (int)r["round"] == round && (string)r["artifact"] == artifact.label)
                        && !notRun.Any(r => (int)r["round"] == round && (string)r["artifact"] == artifact.label))
                        notRun.Add(Map("artifact", artifact.label, "round", round, "status", "not_run_after_harness_failure"));
                }
            }
        }
        finally
        {
            if (job != IntPtr.Zero) CloseHandle(job);
        }
        Report["status"] = passed ? "measured" : "failed";
        Report["finishedAt"] = DateTime.UtcNow.ToString("o"); Phase("complete");
        return passed ? 0 : 1;
    }
}
