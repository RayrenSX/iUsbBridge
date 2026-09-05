using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace iUsbBridgeDemo;

internal static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        ApplicationConfiguration.Initialize();
        var bridgePath = ResolveBridge(args);
        TouchForm? form = null;
        using var bridge = new TouchBridge(bridgePath, message => form?.SetStatus(message));
        form = new TouchForm(bridge, Path.GetFileName(bridgePath));
        form.ShowDialog();
    }

    private static string ResolveBridge(string[] args)
    {
        if (args.Length > 0 && File.Exists(args[0])) return Path.GetFullPath(args[0]);
        var beside = Path.Combine(AppContext.BaseDirectory, "iUsbBridge.exe");
        if (File.Exists(beside)) return beside;
        throw new FileNotFoundException(
            "Put iUsbBridge.exe beside iUsbBridgeDemo.exe, or pass its path as the first argument.",
            beside);
    }
}

internal sealed class TouchBridge : IDisposable
{
    private readonly Process _process;
    private readonly Stream _input;
    private readonly Action<string> _status;
    private volatile bool _ready;
    private long _sequence;

    public TouchBridge(string executable, Action<string> status)
    {
        _status = status;
        _process = new Process
        {
            StartInfo = new ProcessStartInfo
            {
                FileName = executable,
                Arguments = "--usb --rate-hz 120",
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardInput = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                StandardOutputEncoding = Encoding.UTF8,
                StandardErrorEncoding = Encoding.UTF8,
            },
            EnableRaisingEvents = true,
        };
        if (!_process.Start()) throw new InvalidOperationException("Could not start the USB touch bridge.");
        _input = _process.StandardInput.BaseStream;
        _ = ReadEventsAsync(_process.StandardOutput);
        _ = DrainAsync(_process.StandardError);
    }

    public void Send(string phase, double x, double y)
    {
        if (_process.HasExited || !_ready)
        {
            _status("正在连接 iPhone/iPad，请等待状态变为“已就绪”...");
            return;
        }
        var message = JsonSerializer.SerializeToUtf8Bytes(new
        {
            schema = "iphoneMirror.touch.v2",
            kind = "touch_batch",
            seq = Interlocked.Increment(ref _sequence),
            timestampNs = (ulong)Stopwatch.GetTimestamp(),
            points = new[] { new { pointerId = 1, action = phase, normalizedX = x, normalizedY = y } },
        });
        Span<byte> header = stackalloc byte[4];
        BitConverter.TryWriteBytes(header, message.Length);
        _input.Write(header);
        _input.Write(message);
        _input.Flush();
    }

    private async Task ReadEventsAsync(StreamReader reader)
    {
        try
        {
            while (await reader.ReadLineAsync().ConfigureAwait(false) is { } line)
            {
                try
                {
                    using var json = JsonDocument.Parse(line);
                    var root = json.RootElement;
                    var kind = root.TryGetProperty("event", out var eventProp) ? eventProp.GetString() : null;
                    var code = root.TryGetProperty("code", out var codeProp) ? codeProp.GetString() : null;
                    if (kind == "ready")
                    {
                        _ready = true;
                        _status("已就绪，可以在窗口内点击或拖动控制设备");
                    }
                    else if (kind == "error") _status("桥接器错误：" + (root.TryGetProperty("message", out var msg) ? msg.GetString() : code));
                    else if (kind == "status") _status("连接状态：" + (code ?? "working"));
                    else if (kind == "warning") _status("提示：" + (root.TryGetProperty("message", out var warning) ? warning.GetString() : code));
                }
                catch (JsonException) { }
            }
        }
        catch (ObjectDisposedException) { }
    }

    public void Dispose()
    {
        try { _input.Dispose(); } catch { }
        try
        {
            if (!_process.WaitForExit(2000)) _process.Kill(true);
        }
        catch { }
        _process.Dispose();
    }
}

internal sealed class TouchForm : Form
{
    private readonly TouchBridge _bridge;
    private readonly Label _status;
    private bool _pressed;

    public TouchForm(TouchBridge bridge, string bridgeName)
    {
        _bridge = bridge;
        Text = "iUsbBridge Demo";
        ClientSize = new Size(430, 820);
        MinimumSize = new Size(280, 460);
        BackColor = Color.FromArgb(16, 19, 23);
        ForeColor = Color.White;
        DoubleBuffered = true;
        _status = new Label
        {
            Dock = DockStyle.Bottom,
            Height = 34,
            Text = $"iUsbBridge: {bridgeName} | click or drag in this window",
            TextAlign = ContentAlignment.MiddleLeft,
            Padding = new Padding(10, 0, 0, 0),
            BackColor = Color.FromArgb(232, 238, 242),
            ForeColor = Color.FromArgb(25, 35, 42),
        };
        Controls.Add(_status);
        MouseDown += OnMouseDown;
        MouseMove += OnMouseMove;
        MouseUp += OnMouseUp;
        FormClosed += (_, _) => { if (_pressed) _bridge.Send("up", 0.5, 0.5); };
    }

    public void SetStatus(string text)
    {
        if (IsDisposed) return;
        if (InvokeRequired) BeginInvoke(() => SetStatus(text));
        else _status.Text = text;
    }

    private (double X, double Y) Normalize(Point point)
    {
        var width = Math.Max(1, ClientSize.Width);
        var height = Math.Max(1, ClientSize.Height - _status.Height);
        return (Math.Clamp((double)point.X / width, 0, 1),
            Math.Clamp((double)point.Y / height, 0, 1));
    }

    private void OnMouseDown(object? sender, MouseEventArgs e)
    {
        if (e.Button != MouseButtons.Left) return;
        _pressed = true;
        var p = Normalize(e.Location);
        _bridge.Send("down", p.X, p.Y);
    }

    private void OnMouseMove(object? sender, MouseEventArgs e)
    {
        if (!_pressed || e.Button != MouseButtons.Left) return;
        var p = Normalize(e.Location);
        _bridge.Send("move", p.X, p.Y);
    }

    private void OnMouseUp(object? sender, MouseEventArgs e)
    {
        if (!_pressed || e.Button != MouseButtons.Left) return;
        _pressed = false;
        var p = Normalize(e.Location);
        _bridge.Send("up", p.X, p.Y);
    }
}
