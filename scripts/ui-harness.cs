// Hidden desktop UI probe. It never calls SwitchDesktop or sends global input.
// Build with .NET Framework csc and UIAutomationClient/Types/WindowsBase references.
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Automation;

internal static class HiddenUiProbe
{
    static readonly JavaScriptSerializer Json = new JavaScriptSerializer { MaxJsonLength = 16 * 1024 * 1024 };
    const uint CreateSuspended = 0x4, CreateNoWindow = 0x08000000;
    const uint WaitObject0 = 0, WaitTimeout = 258, WmClose = 0x10;
    const int TimeoutMs = 90000;
    static string Output;
    static Dictionary<string, object> Report;

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

    delegate bool EnumWindow(IntPtr window, IntPtr argument);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr mode, uint flags, uint access, IntPtr security);
    [DllImport("user32.dll", SetLastError = true)] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool GetUserObjectInformationW(IntPtr value, int index, StringBuilder output, int length, out int needed);
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
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetInformationJobObject(IntPtr job, int type, ref ExtendedLimit value, uint size);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindow callback, IntPtr parameter);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr window);
    [DllImport("user32.dll", SetLastError = true)] static extern bool AdjustWindowRectExForDpi(ref Rect rect, uint style, bool menu, uint extended, uint dpi);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongW")] static extern int GetWindowLong(IntPtr window, int index);
    [DllImport("user32.dll", SetLastError = true)] static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll", SetLastError = true)] static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern uint RegisterWindowMessageW(string name);
    [DllImport("user32.dll", SetLastError = true)] static extern bool PostMessageW(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll")] static extern uint GetGuiResources(IntPtr process, uint flags);
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr context);

    static Dictionary<string, object> Map(params object[] pairs)
    {
        var result = new Dictionary<string, object>();
        for (int i = 0; i < pairs.Length; i += 2) result[(string)pairs[i]] = pairs[i + 1];
        return result;
    }
    static void Save(string name, object value)
    {
        File.WriteAllText(Path.Combine(Output, name), Json.Serialize(value), new UTF8Encoding(false));
    }
    static void Phase(string phase)
    {
        Report["phase"] = phase;
        Report["updatedAt"] = DateTime.UtcNow.ToString("o");
        Save("worker-result.json", Report);
    }
    static string Quote(string value)
    {
        // Windows argv quoting; no command shell participates.
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
        if (desktop == IntPtr.Zero) return "unavailable:" + Marshal.GetLastWin32Error();
        try
        {
            int required;
            var name = new StringBuilder(256);
            return GetUserObjectInformationW(desktop, 2, name, name.Capacity * 2, out required)
                ? name.ToString() : "unavailable:" + Marshal.GetLastWin32Error();
        }
        finally { CloseDesktop(desktop); }
    }
    static void Require(bool condition, string operation)
    {
        if (!condition) throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
    }

    [MTAThread]
    static int Main(string[] args)
    {
        if (args.Length != 4 || (args[0] != "--controller" && args[0] != "--worker")) return 2;
        SetErrorMode(0x0001 | 0x0002 | 0x8000);
        Output = Path.GetFullPath(args[2]);
        Directory.CreateDirectory(Output);
        return args[0] == "--worker" ? Worker(Path.GetFullPath(args[1]), args[3])
            : Controller(Path.GetFullPath(args[1]), args[3]);
    }

    static int Controller(string app, string namespaceValue)
    {
        var result = Map("schemaVersion", 1, "status", "running", "startedAt", DateTime.UtcNow.ToString("o"),
            "desktopSwitched", false, "timeoutMs", TimeoutMs, "inputDesktopBefore", InputDesktop(),
            "workerExitCode", null, "scope", "hidden desktop settings UIA and client capture");
        IntPtr desktop = IntPtr.Zero, job = IntPtr.Zero;
        ProcessInfo child = new ProcessInfo();
        int exitCode = 1;
        try
        {
            string name = "OHV_" + namespaceValue;
            result["desktopName"] = name;
            Save("controller-result.json", result);
            // Access excludes DESKTOP_SWITCHDESKTOP; there is no switching API in this harness.
            desktop = CreateDesktopW(name, IntPtr.Zero, IntPtr.Zero, 0, 0x00FF, IntPtr.Zero);
            Require(desktop != IntPtr.Zero, "CreateDesktopW");
            job = CreateJobObjectW(IntPtr.Zero, null);
            Require(job != IntPtr.Zero, "CreateJobObjectW");
            var limits = new ExtendedLimit();
            limits.basic.flags = 0x00002000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            Require(SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(typeof(ExtendedLimit))), "SetInformationJobObject");
            string executable = Process.GetCurrentProcess().MainModule.FileName;
            var startup = new StartupInfo { cb = Marshal.SizeOf(typeof(StartupInfo)), desktop = @"WinSta0\" + name };
            var command = new StringBuilder(Quote(executable) + " --worker " + Quote(app) + " " + Quote(Output) + " " + Quote(namespaceValue));
            Require(CreateProcessW(executable, command, IntPtr.Zero, IntPtr.Zero, false,
                CreateSuspended | CreateNoWindow, IntPtr.Zero, Output, ref startup, out child), "CreateProcessW worker");
            result["workerPid"] = child.processId;
            Require(AssignProcessToJobObject(job, child.process), "AssignProcessToJobObject worker");
            Require(ResumeThread(child.thread) != UInt32.MaxValue, "ResumeThread worker");
            CloseHandle(child.thread); child.thread = IntPtr.Zero;
            uint waited = WaitForSingleObject(child.process, TimeoutMs);
            if (waited == WaitTimeout) throw new TimeoutException("UI worker timed out; inspect worker-result.json phase. Its job will be closed.");
            Require(waited == WaitObject0, "WaitForSingleObject worker");
            uint workerCode;
            Require(GetExitCodeProcess(child.process, out workerCode), "GetExitCodeProcess");
            result["workerExitCode"] = workerCode;
            if (workerCode != 0) throw new InvalidOperationException("UI worker reported failure.");
            if (!File.Exists(Path.Combine(Output, "worker-result.json"))) throw new InvalidOperationException("Missing worker result.");
            result["status"] = "passed";
            exitCode = 0;
        }
        catch (Exception error) { result["status"] = "failed"; result["error"] = error.ToString(); }
        finally
        {
            // Only our job and exact worker handles are ever terminated.
            if (job != IntPtr.Zero) CloseHandle(job);
            if (child.process != IntPtr.Zero)
            {
                if (WaitForSingleObject(child.process, 2000) == WaitTimeout) TerminateProcess(child.process, 1);
                WaitForSingleObject(child.process, 5000);
                CloseHandle(child.process);
            }
            if (child.thread != IntPtr.Zero) CloseHandle(child.thread);
            if (desktop != IntPtr.Zero) result["desktopClosed"] = CloseDesktop(desktop);
            result["inputDesktopAfter"] = InputDesktop();
            result["finishedAt"] = DateTime.UtcNow.ToString("o");
            Save("controller-result.json", result);
        }
        return exitCode;
    }

    static IntPtr FindWindowFor(int pid)
    {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr window, IntPtr parameter)
        {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            Rect rect;
            if (owner == (uint)pid && IsWindowVisible(window) && GetClientRect(window, out rect)
                && rect.right - rect.left > 100 && rect.bottom - rect.top > 100)
            { found = window; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    static bool Until(Func<bool> condition, int milliseconds)
    {
        var time = Stopwatch.StartNew();
        using (var wake = new ManualResetEvent(false))
        {
            do
            {
                if (condition()) return true;
                // Bounded condition polling; elapsed time alone is never success.
                wake.WaitOne(50);
            } while (time.ElapsedMilliseconds < milliseconds);
        }
        return false;
    }
    static Dictionary<string, object> Metrics(Process process)
    {
        process.Refresh();
        return Map("at", DateTime.UtcNow.ToString("o"), "privateBytes", process.PrivateMemorySize64,
            "workingSetBytes", process.WorkingSet64, "cpuMs", process.TotalProcessorTime.TotalMilliseconds,
            "gdi", GetGuiResources(process.Handle, 0), "user", GetGuiResources(process.Handle, 1), "handles", process.HandleCount);
    }
    static object Finite(double value) { return Double.IsNaN(value) || Double.IsInfinity(value) ? null : (object)value; }
    static List<object> Uia(IntPtr window)
    {
        var root = AutomationElement.FromHandle(window);
        var elements = root.FindAll(TreeScope.Subtree, Condition.TrueCondition);
        if (elements.Count > 4096) throw new InvalidOperationException("UIA tree exceeds fixture bound.");
        var nodes = new List<object>();
        for (int i = 0; i < elements.Count; i++)
        {
            var value = elements[i].Current;
            var rect = value.BoundingRectangle;
            nodes.Add(Map("name", value.Name, "automationId", value.AutomationId, "role", value.ControlType.ProgrammaticName,
                "enabled", value.IsEnabled, "focusable", value.IsKeyboardFocusable, "focused", value.HasKeyboardFocus,
                "offscreen", value.IsOffscreen, "x", Finite(rect.X), "y", Finite(rect.Y), "width", Finite(rect.Width), "height", Finite(rect.Height)));
        }
        return nodes;
    }
    static bool Capture(IntPtr window, string path, out int colors)
    {
        Rect client;
        Require(GetClientRect(window, out client), "GetClientRect capture");
        int width = client.right - client.left, height = client.bottom - client.top;
        if (width <= 0 || height <= 0 || width > 4096 || height > 4096) throw new InvalidOperationException("Invalid capture dimensions.");
        using (var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb))
        {
            bool printed;
            using (var graphics = Graphics.FromImage(bitmap))
            {
                graphics.Clear(Color.Magenta); // Unpainted PrintWindow buffers remain detectable.
                IntPtr dc = graphics.GetHdc();
                try { printed = PrintWindow(window, dc, 3); } // CLIENTONLY | RENDERFULLCONTENT
                finally { graphics.ReleaseHdc(dc); }
            }
            var distinct = new HashSet<int>();
            for (int y = 8; y < height - 8; y += 4)
                for (int x = 8; x < width - 8; x += 4) distinct.Add(bitmap.GetPixel(x, y).ToArgb());
            colors = distinct.Count;
            bitmap.Save(path, ImageFormat.Png);
            return printed && colors >= 12;
        }
    }


    static bool CaptureFramebuffer(IntPtr window, string directory, string path, int token, out int colors)
    {
        uint message = RegisterWindowMessageW("OpenHinoVoice.Test.SettingsCapture.v1");
        Require(message != 0, "RegisterWindowMessage capture");
        Require(PostMessageW(window, message, new IntPtr(token), IntPtr.Zero), "PostMessage capture");
        string metadata = Path.Combine(directory, "capture-" + token + ".json");
        if (!Until(delegate { return File.Exists(metadata); }, 10000))
            throw new TimeoutException("Egui framebuffer callback did not complete before its deadline.");
        var report = Json.Deserialize<Dictionary<string, object>>(File.ReadAllText(metadata));
        if ((string)report["status"] != "captured")
            throw new InvalidOperationException("Framebuffer capture failed: " + Convert.ToString(report["error"]));
        string bitmapPath = Path.Combine(directory, "capture-" + token + ".bmp");
        using (var bitmap = new Bitmap(bitmapPath))
        {
            Rect client;
            Require(GetClientRect(window, out client), "GetClientRect framebuffer");
            if (bitmap.Width != client.right - client.left || bitmap.Height != client.bottom - client.top)
                throw new InvalidOperationException("Framebuffer dimensions do not match the resized client.");
            var distinct = new HashSet<int>();
            for (int y = 8; y < bitmap.Height - 8; y += 4)
                for (int x = 8; x < bitmap.Width - 8; x += 4) distinct.Add(bitmap.GetPixel(x, y).ToArgb());
            colors = distinct.Count;
            bitmap.Save(path, ImageFormat.Png);
            return colors >= 12;
        }
    }

    static int Worker(string app, string namespaceValue)
    {
        SetProcessDpiAwarenessContext(new IntPtr(-4));
        var cases = new List<object>();
        Report = Map("schemaVersion", 1, "status", "running", "startedAt", DateTime.UtcNow.ToString("o"), "cases", cases,
            "audioRequested", false, "microphoneRequested", false, "globalInputSent", false,
            "limitations", new[] { "Capture success requires human inspection; nonblank is only a transport check.",
                "Native DPI is the existing Windows value; no user display settings are changed.",
                "No keyboard shortcut, preview audio, ASR, paste, or external API validation is performed." });
        Phase("starting");
        bool passed = true;
        foreach (var size in new[] { new[] { 600, 420 }, new[] { 720, 520 } })
        {
            string label = size[0] + "x" + size[1];
            string directory = Path.Combine(Output, label);
            Directory.CreateDirectory(directory);
            var result = Map("sizeLogical", label, "status", "running", "pid", null, "gracefulExit", false);
            cases.Add(result);
            Process process = null;
            System.Threading.Tasks.Task<string> stdout = null, stderr = null;
            IntPtr window = IntPtr.Zero;
            try
            {
                Phase(label + ":launch");
                string appdata = Path.Combine(directory, "appdata");
                string local = Path.Combine(directory, "localappdata");
                Directory.CreateDirectory(Path.Combine(appdata, "OpenHinoVoice"));
                Directory.CreateDirectory(local);
                string captureDirectory = Path.Combine(local, "OpenHinoVoice", "ui-captures", namespaceValue + "-" + label);
                Directory.CreateDirectory(captureDirectory);
                File.WriteAllText(Path.Combine(appdata, "OpenHinoVoice", "config.toml"),
                    "speech_exe = \"missing-ui-fixture-speech.exe\"\nsounds = false\nvolume = 0\nstartup_sound = false\nhud_style = \"rich\"\n[format]\noffline = true\n", new UTF8Encoding(false));
                var start = new ProcessStartInfo(app, "--settings") { UseShellExecute = false, CreateNoWindow = true,
                    WorkingDirectory = directory, RedirectStandardOutput = true, RedirectStandardError = true };
                start.EnvironmentVariables["APPDATA"] = appdata;
                start.EnvironmentVariables["LOCALAPPDATA"] = local;
                start.EnvironmentVariables["OHV_TEST_NAMESPACE"] = namespaceValue + "-" + label;
                start.EnvironmentVariables["OHV_TEST_SETTINGS_PAGE"] = "display";
                start.EnvironmentVariables["OHV_TEST_SETTINGS_SIZE"] = label;
                start.EnvironmentVariables["OHV_TEST_SETTINGS_CAPTURE_DIR"] = captureDirectory;
                process = Process.Start(start);
                if (process == null) throw new InvalidOperationException("Settings did not start.");
                result["pid"] = process.Id;
                stdout = process.StandardOutput.ReadToEndAsync();
                stderr = process.StandardError.ReadToEndAsync();
                Phase(label + ":await_window");
                if (!Until(delegate { if (process.HasExited) return true; window = FindWindowFor(process.Id); return window != IntPtr.Zero; }, 15000)
                    || process.HasExited || window == IntPtr.Zero) throw new InvalidOperationException("Visible settings window absent on the hidden desktop.");
                uint dpi = GetDpiForWindow(window);
                if (dpi == 0) throw new InvalidOperationException("Could not read native window DPI.");
                int width = (int)Math.Round(size[0] * dpi / 96.0), height = (int)Math.Round(size[1] * dpi / 96.0);
                var rect = new Rect { right = width, bottom = height };
                Require(AdjustWindowRectExForDpi(ref rect, (uint)GetWindowLong(window, -16), false,
                    (uint)GetWindowLong(window, -20), dpi), "AdjustWindowRectExForDpi");
                Require(SetWindowPos(window, IntPtr.Zero, 0, 0, rect.right - rect.left, rect.bottom - rect.top, 0x16), "SetWindowPos");
                if (!Until(delegate { Rect actual; return GetClientRect(window, out actual) && actual.right == width && actual.bottom == height; }, 5000))
                    throw new InvalidOperationException("Settings client did not reach the requested logical size.");
                result["dpi"] = dpi;
                result["clientPixels"] = new[] { width, height };
                result["metricsBefore"] = Metrics(process);
                Phase(label + ":uia");
                var nodes = new List<object>();
                bool displayPage = false;
                int focusable = 0;
                string lastUiaError = null;
                bool uiaReady = Until(delegate {
                    try {
                        nodes = Uia(window);
                        Save(Path.Combine(label, "uia.json"), nodes);
                        displayPage = nodes.OfType<Dictionary<string, object>>().Any(n => (string)n["name"] == "試す" && !(bool)n["offscreen"]);
                        focusable = nodes.OfType<Dictionary<string, object>>().Count(n => (bool)n["focusable"]);
                        return nodes.Count >= 10 && displayPage && focusable > 0;
                    } catch (Exception error) { lastUiaError = error.Message; return false; }
                }, 10000);
                result["uiaNodes"] = nodes.Count;
                result["displayPageNamed"] = displayPage;
                result["focusableNodes"] = focusable;
                result["uiaStatus"] = uiaReady ? "passed" : "failed";
                if (!uiaReady) {
                    passed = false;
                    result["status"] = "failed";
                    result["uiaError"] = lastUiaError ?? "UIA did not expose the expected settings controls before its deadline.";
                }
                Phase(label + ":capture");
                int uniqueColors = 0;
                string printWindowPath = Path.Combine(directory, "client-printwindow.png");
                bool printWindow = Capture(window, printWindowPath, out uniqueColors);
                result["printWindow"] = Map("status", printWindow ? "passed" : "failed", "colors", uniqueColors, "path", label + "/client-printwindow.png");
                string capture = Path.Combine(directory, "client-framebuffer.png");
                bool captured = CaptureFramebuffer(window, captureDirectory, capture, 1, out uniqueColors);
                result["captureColors"] = uniqueColors;
                result["capturePath"] = label + "/client-framebuffer.png";
                result["captureRenderer"] = "eframe glow framebuffer";
                if (!captured) throw new InvalidOperationException("Egui framebuffer capture produced a blank client image.");
                result["metricsAfter"] = Metrics(process);
                if (uiaReady) result["status"] = "passed";
            }
            catch (Exception error) { passed = false; result["status"] = "failed"; result["error"] = error.ToString(); }
            finally
            {
                Phase(label + ":close");
                if (process != null)
                {
                    if (!process.HasExited)
                    {
                        if (window != IntPtr.Zero) PostMessageW(window, WmClose, IntPtr.Zero, IntPtr.Zero);
                        bool closed = process.WaitForExit(5000);
                        result["gracefulExit"] = closed;
                        if (!closed)
                        {
                            passed = false; result["status"] = "failed"; result["closeError"] = "Settings did not exit after WM_CLOSE.";
                            process.Kill();
                            process.WaitForExit(5000);
                        }
                    }
                    result["exitCode"] = process.HasExited ? (object)process.ExitCode : null;
                    if (stdout != null && stdout.Wait(1000)) File.WriteAllText(Path.Combine(directory, "stdout.log"), stdout.Result, new UTF8Encoding(false));
                    if (stderr != null && stderr.Wait(1000)) File.WriteAllText(Path.Combine(directory, "stderr.log"), stderr.Result, new UTF8Encoding(false));
                    process.Dispose();
                }
                Phase(label + ":complete");
            }
        }
        Report["status"] = passed ? "passed" : "failed";
        Report["finishedAt"] = DateTime.UtcNow.ToString("o");
        Phase("complete");
        return passed ? 0 : 1;
    }
}
