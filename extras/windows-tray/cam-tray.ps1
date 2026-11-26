# cam 的 Windows 托盘小工具：任务栏右下角的图标显示当前 Claude Code 账号的状态。样式在右键菜单“更多”或卡片小面板的设置里选：
# 数字方块（写着 5 小时额度用了百分之几，底色 70% 以下绿、70–89% 黄、90% 以上红），或者和 macOS 菜单栏一样的信号格、红绿灯、
# 圆环、双环、两根小条。Claude Code 在回答时图标一闪一闪，等你确认时整个图标快闪。左键开卡片小窗，右键菜单里点一下切换。
# 菜单内容和 macOS 菜单栏小工具一样，由 cam_tray.py 每分钟算一次，顺带做后台任务（自动保存新登录、时间规则、
# 等待中的切换、刷新用量、额度提醒）；这个脚本只画图标和菜单、把点击交给 cam_tray.py、弹通知。
#
# 一键安装（安装.cmd）时选了装托盘小工具，就会放进“启动”文件夹，登录 Windows 后自动运行。手动运行：
#   conhost.exe --headless powershell.exe -NoProfile -ExecutionPolicy Bypass -File extras\windows-tray\cam-tray.ps1
# 同一时间只会有一个在运行；这个文件更新后（cam update）会自己重启。
param([string]$Python = "")

# python 往 stderr 写的正常输出在 Stop 模式下会被当成错误，所以只看退出码
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace CamTray -Name Native -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr handle);
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr handle, int cmd);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr handle, uint msg, IntPtr w, IntPtr l);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr handle);
'@

# 右键菜单的样子（像 Windows 11 的菜单：圆角、细分隔线、绿色的勾、浅色 / 深色跟着系统）和账号行下面的额度条。
# Windows PowerShell 5.1 的 Add-Type 用老的 C# 编译器：只能写 C# 5（没有 $"" 字符串、=> 成员、?.）
function Add-MenuLook {
  if ("CamTray.MenuRenderer" -as [type]) { return }
  Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -IgnoreWarnings -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;
using System.Windows.Forms;

namespace CamTray {
  // 账号行：名字下面画一根 5 小时的额度条
  public class BarItem : ToolStripMenuItem {
    [DesignerSerializationVisibility(DesignerSerializationVisibility.Hidden)]
    public double Bar { get; set; }
    [DesignerSerializationVisibility(DesignerSerializationVisibility.Hidden)]
    public Color BarColor { get; set; }
    public BarItem(string text) : base(text) { Bar = -1; BarColor = Color.Empty; }
  }

  // 像 Windows 11 的菜单：浅色 / 深色两套颜色，圆角高亮，细分隔线，绿色的勾
  public class MenuRenderer : ToolStripProfessionalRenderer {
    public readonly bool Dark;
    readonly Color back, border, hover, text, muted, line, check, track;

    public MenuRenderer(bool dark) : base() {
      Dark = dark;
      RoundedEdges = false;
      back = dark ? Color.FromArgb(44, 44, 44) : Color.FromArgb(249, 249, 249);
      border = dark ? Color.FromArgb(66, 66, 66) : Color.FromArgb(222, 222, 222);
      hover = dark ? Color.FromArgb(61, 61, 61) : Color.FromArgb(234, 234, 234);
      text = dark ? Color.FromArgb(242, 242, 242) : Color.FromArgb(28, 28, 30);
      muted = dark ? Color.FromArgb(160, 160, 165) : Color.FromArgb(110, 110, 115);
      line = dark ? Color.FromArgb(70, 70, 70) : Color.FromArgb(226, 226, 226);
      check = dark ? Color.FromArgb(76, 194, 122) : Color.FromArgb(31, 138, 76);
      track = dark ? Color.FromArgb(80, 80, 82) : Color.FromArgb(224, 224, 226);
    }

    public Color TextColor { get { return text; } }

    static GraphicsPath Round(RectangleF r, float rad) {
      GraphicsPath p = new GraphicsPath();
      float d = rad * 2;
      p.AddArc(r.Left, r.Top, d, d, 180, 90);
      p.AddArc(r.Right - d, r.Top, d, d, 270, 90);
      p.AddArc(r.Right - d, r.Bottom - d, d, d, 0, 90);
      p.AddArc(r.Left, r.Bottom - d, d, d, 90, 90);
      p.CloseFigure();
      return p;
    }

    static float Px(Graphics g, float v) {
      return v * g.DpiX / 96f;  // 按屏幕缩放放大（高分屏上 96 dpi 的 4 像素太细）
    }

    protected override void OnRenderToolStripBackground(ToolStripRenderEventArgs e) {
      using (SolidBrush b = new SolidBrush(back)) { e.Graphics.FillRectangle(b, e.AffectedBounds); }
    }

    protected override void OnRenderImageMargin(ToolStripRenderEventArgs e) {
      // 勾的那一栏和背景同色，不画分界
    }

    protected override void OnRenderToolStripBorder(ToolStripRenderEventArgs e) {
      Rectangle r = new Rectangle(0, 0, e.ToolStrip.Width - 1, e.ToolStrip.Height - 1);
      using (Pen p = new Pen(border)) { e.Graphics.DrawRectangle(p, r); }
    }

    protected override void OnRenderMenuItemBackground(ToolStripItemRenderEventArgs e) {
      ToolStripItem it = e.Item;
      if (!it.Selected || !it.Enabled || it.AccessibleRole == AccessibleRole.StaticText) { return; }
      Graphics g = e.Graphics;
      g.SmoothingMode = SmoothingMode.AntiAlias;
      float m = Px(g, 4);
      RectangleF r = new RectangleF(m, 1, it.Width - 2 * m, it.Height - 2);
      using (GraphicsPath path = Round(r, Px(g, 4))) {
        using (SolidBrush b = new SolidBrush(hover)) { g.FillPath(b, path); }
      }
    }

    protected override void OnRenderSeparator(ToolStripSeparatorRenderEventArgs e) {
      int y = e.Item.Height / 2;
      int m = (int)Px(e.Graphics, 10);
      using (Pen p = new Pen(line)) { e.Graphics.DrawLine(p, m, y, e.Item.Width - m, y); }
    }

    protected override void OnRenderArrow(ToolStripArrowRenderEventArgs e) {
      e.ArrowColor = muted;
      base.OnRenderArrow(e);
    }

    protected override void OnRenderItemCheck(ToolStripItemImageRenderEventArgs e) {
      Graphics g = e.Graphics;
      g.SmoothingMode = SmoothingMode.AntiAlias;
      Rectangle r = e.ImageRectangle;
      float s = Math.Min(r.Width, r.Height) * 0.75f;
      float x = r.Left + (r.Width - s) / 2, y = r.Top + (r.Height - s) / 2;
      using (Pen p = new Pen(check, Math.Max(1.6f, s / 7f))) {
        p.StartCap = LineCap.Round;
        p.EndCap = LineCap.Round;
        p.LineJoin = LineJoin.Round;
        g.DrawLines(p, new PointF[] {
          new PointF(x + s * 0.12f, y + s * 0.55f), new PointF(x + s * 0.4f, y + s * 0.82f), new PointF(x + s * 0.9f, y + s * 0.22f) });
      }
    }

    protected override void OnRenderItemText(ToolStripItemTextRenderEventArgs e) {
      ToolStripMenuItem mi = e.Item as ToolStripMenuItem;
      bool shortcut = mi != null && !string.IsNullOrEmpty(mi.ShortcutKeyDisplayString) && e.Text == mi.ShortcutKeyDisplayString && e.Text != mi.Text;
      Color c = e.Item.ForeColor;
      if (!e.Item.Enabled || shortcut) {
        c = muted;
      } else if (c == SystemColors.ControlText || c == SystemColors.MenuText || c == Color.Empty) {
        c = text;
      }
      e.TextColor = c;
      base.OnRenderItemText(e);
      BarItem bi = e.Item as BarItem;
      if (bi == null || shortcut || bi.Bar < 0) { return; }
      Graphics g = e.Graphics;
      g.SmoothingMode = SmoothingMode.AntiAlias;
      float h = Px(g, 3);
      float left = e.TextRectangle.Left + 1;
      float right = e.Item.Width - Px(g, 14);
      float y = e.Item.Height - Px(g, 6);
      if (right - left < 10) { return; }
      RectangleF all = new RectangleF(left, y, right - left, h);
      using (GraphicsPath p = Round(all, h / 2)) {
        using (SolidBrush b = new SolidBrush(track)) { g.FillPath(b, p); }
      }
      float w = (float)Math.Max(h, (right - left) * Math.Min(100, Math.Max(0, bi.Bar)) / 100);
      using (GraphicsPath p = Round(new RectangleF(left, y, w, h), h / 2)) {
        using (SolidBrush b = new SolidBrush(bi.BarColor.IsEmpty ? check : bi.BarColor)) { g.FillPath(b, p); }
      }
    }
  }

  public static class Look {
    [DllImport("dwmapi.dll")]
    static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int value, int size);

    // Windows 11：菜单窗口画成圆角（系统自己加阴影）；Windows 10 没有这个属性，调用失败也不影响
    public static void Round(ToolStripDropDown dd) {
      dd.Opened += delegate(object s, EventArgs a) {
        try {
          int v = 2;  // DWMWCP_ROUND
          DwmSetWindowAttribute(dd.Handle, 33, ref v, 4);  // DWMWA_WINDOW_CORNER_PREFERENCE
        } catch (Exception) { }
      };
    }
  }
}
'@
}
Add-MenuLook

$created = $false
$Mutex = New-Object System.Threading.Mutex($true, "Local\cam-tray", [ref]$created)
if (-not $created) { exit 0 }

# 不是用 conhost --headless 启动的话，至少把自己的控制台窗口藏起来
[void][CamTray.Native]::ShowWindow([CamTray.Native]::GetConsoleWindow(), 0)
[void][CamTray.Native]::SetProcessDPIAware()  # 高分屏上图标按实际大小画，不发虚
[System.Windows.Forms.Application]::EnableVisualStyles()
# 兜底：事件处理里漏接的异常不弹 WinForms 的“未处理的异常”对话框（那个框点“退出”托盘就没了），记成一条通知，托盘照常运行。
# 要在建任何窗口、菜单之前设
try { [System.Windows.Forms.Application]::SetUnhandledExceptionMode([System.Windows.Forms.UnhandledExceptionMode]::CatchException) } catch { }
[System.Windows.Forms.Application]::add_ThreadException({
  param($s, $e)
  try { Add-Note "托盘小工具出错了" $e.Exception.Message } catch { }
})

$Here = $PSScriptRoot
$TrayPy = Join-Path $Here "cam_tray.py"
$Self = $PSCommandPath
$SelfTime = (Get-Item -LiteralPath $Self).LastWriteTimeUtc

# ---- Python：和 install.ps1 一样依次试 py -3、python、python3；Microsoft Store 的占位程序要真的运行一下才知道能不能用
function Find-Python {
  if ($Python) { return , @($Python) }
  foreach ($cand in "py -3", "python", "python3") {
    $parts = $cand -split " "
    $cmd = Get-Command $parts[0] -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cmd) { continue }
    $extra = @($parts | Select-Object -Skip 1)
    & $cmd.Source @extra -c "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)" 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) { return , (@($cmd.Source) + $extra) }
  }
  return $null
}
$Py = Find-Python
if (-not $Py) {
  [void][System.Windows.Forms.MessageBox]::Show("没找到 Python 3.8 以上，托盘小工具没法运行。`n重新运行`“安装.cmd`”可以自动装好 Python。", "cam 托盘小工具")
  exit 1
}

# ---- 在后台运行 cam_tray.py，不弹黑窗口、不卡住菜单
# Windows PowerShell 的 ProcessStartInfo 只能传一整串参数，按 Windows 的规则给每个参数加引号
function ConvertTo-Arg([string]$a) {
  if ($a -and $a -notmatch '[\s"]') { return $a }
  $s = '"'
  $bs = 0
  foreach ($ch in $a.ToCharArray()) {
    if ($ch -eq '\') { $bs++; continue }
    if ($ch -eq '"') { $s += ('\' * (2 * $bs + 1)) + '"' } else { $s += ('\' * $bs) + $ch }
    $bs = 0
  }
  return $s + ('\' * (2 * $bs)) + '"'
}

# 中转、API Key 这类变量（和 cam.py 的 SCAN_ENV 一致）：托盘是开机时启动的，身上带着那时候的环境变量。一键清理从系统里删掉以后，
# 起 Python 时把系统里（用户、系统环境变量）已经没有的去掉，托盘这边就不会一直带着旧值
$RelayEnv = @("ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_CUSTOM_HEADERS", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY", "ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL", "ANTHROPIC_SMALL_FAST_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL", "NODE_TLS_REJECT_UNAUTHORIZED", "DISABLE_AUTOUPDATER")

$script:jobs = @()
function Start-TrayPy([string[]]$argv, [string]$kind, [bool]$refresh) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $Py[0]
  $all = @($Py | Select-Object -Skip 1) + @($TrayPy) + $argv
  $psi.Arguments = (@($all | ForEach-Object { ConvertTo-Arg $_ })) -join " "
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardInput = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
  $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
  $psi.EnvironmentVariables["PYTHONIOENCODING"] = "utf-8"
  foreach ($k in $RelayEnv) {
    if ($psi.EnvironmentVariables.ContainsKey($k) -and -not [Environment]::GetEnvironmentVariable($k, "User") -and
        -not [Environment]::GetEnvironmentVariable($k, "Machine")) { $psi.EnvironmentVariables.Remove($k) }
  }
  try {
    $p = [System.Diagnostics.Process]::Start($psi)
  } catch {
    Add-Note "托盘小工具出错了" "运行 Python 失败：$($_.Exception.Message)"
    if ($kind -eq "menu") { $script:menuBusy = $false }
    return
  }
  $p.StandardInput.Close()
  $script:jobs += @{
    proc = $p; kind = $kind; refresh = $refresh; started = Get-Date
    out = $p.StandardOutput.ReadToEndAsync(); err = $p.StandardError.ReadToEndAsync()
  }
}

# 菜单每分钟算一次；正在算的时候又要算（点了切换、刷新），等这次算完再来一次
$script:menuBusy = $false
$script:menuAgain = $false
function Request-Menu {
  if ($script:menuBusy) { $script:menuAgain = $true; return }
  $script:menuBusy = $true
  Start-TrayPy @("menu", "$IconPx") "menu" $false
}

# 开着 Claude Code 会话（cam_tray.py 说 watch）时每 20 秒只看一下状态，图标的动画跟得上；菜单还是每分钟算一次
$script:watch = $false
$script:lightBusy = $false
function Request-Light {
  if (-not $script:watch -or $script:lightBusy -or $script:menuBusy) { return }
  $script:lightBusy = $true
  Start-TrayPy @("light", "$IconPx") "light" $false
}

function Complete-Job($j) {
  try {
    $res = $null
    # cam_tray.py 最后一行是 JSON
    $rows = @(($j.out.Result -split "`r?`n") | Where-Object { $_.Trim() })
    if ($rows.Count) { try { $res = $rows[-1] | ConvertFrom-Json } catch { $res = $null } }
    if ($j.kind -eq "light") {
      # 只看状态的那次出错不弹通知（20 秒一次会一直弹），下一分钟的菜单会报
      if ($res -and $res.icon) { Update-Icon $res }
    } elseif (-not $res) {
      $lines = @(($j.err.Result -split "`r?`n") | Where-Object { $_.Trim() })
      $why = if ($lines.Count) { $lines[-1] } else { "没有输出（退出码 $($j.proc.ExitCode)）" }
      Add-Note "托盘小工具出错了" $why
    } else {
      foreach ($n in @($res.notes)) { if ($n) { Add-Note $n.title $n.body } }
      if ($j.kind -eq "menu") { Update-Tray $res }
    }
  } catch {
    # 读输出、画图标出了意外也要把“正在算”的状态清掉，不然菜单再也不刷新
    Add-Note "托盘小工具出错了" $_.Exception.Message
  }
  if ($j.kind -eq "menu") {
    $script:menuBusy = $false
    if ($script:menuAgain) { $script:menuAgain = $false; Request-Menu }
  } elseif ($j.kind -eq "light") {
    $script:lightBusy = $false
  } elseif ($j.refresh) {
    Request-Menu
  }
}

# ---- 通知：用托盘图标的气泡（Windows 10 / 11 上显示成系统通知）。连着来的几条间隔几秒依次弹
$script:notes = New-Object System.Collections.Queue
$script:lastNote = [datetime]::MinValue
function Add-Note([string]$title, [string]$body) {
  if (-not $title) { $title = "cam" }
  if (-not $body) { $body = $title }
  $script:notes.Enqueue(@($title, $body))
}
function Show-NextNote {
  if (-not $script:notes.Count -or ((Get-Date) - $script:lastNote).TotalSeconds -lt 5) { return }
  $n = $script:notes.Dequeue()
  $t = $n[0]; $b = $n[1]
  if ($t.Length -gt 63) { $t = $t.Substring(0, 62) + "…" }
  if ($b.Length -gt 255) { $b = $b.Substring(0, 254) + "…" }
  $script:notify.ShowBalloonTip(8000, $t, $b, [System.Windows.Forms.ToolTipIcon]::None)
  $script:lastNote = Get-Date
}

# ---- 图标。托盘图标的实际像素（屏幕缩放 100% 是 16，150% 是 24），cam_tray.py 按它画，不再缩放，高分屏上不发虚
$IconPx = [System.Windows.Forms.SystemInformation]::SmallIconSize.Width

# 位图转成托盘图标。图标句柄要自己释放（Remove-Frames），不然每换一次漏一个，几天后托盘就画不出来了
function New-Frame($bmp) {
  $h = $bmp.GetHicon()
  return @{ icon = [System.Drawing.Icon]::FromHandle($h); handle = $h }
}

# 数字方块：圆角方块，写着百分比，底色跟着额度变。$bg 是底色的不透明度（工作中暗拍留 0.35），$all 是整个图标的（等你确认暗拍 0.25）
function New-BadgeFrame([string]$level, [string]$badge, [double]$bg = 1.0, [double]$all = 1.0) {
  $size = $IconPx
  $hex = switch ($level) { "green" { "#1F8A4C" } "yellow" { "#B87A08" } "red" { "#BF3B33" } default { "#6E6E73" } }
  $base = [System.Drawing.ColorTranslator]::FromHtml($hex)
  $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.Clear([System.Drawing.Color]::Transparent)
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
  $brush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb([int](255 * $bg * $all), $base))
  $ink = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb([int](255 * $all), 255, 255, 255))
  $d = [single][Math]::Max(4.0, $size / 2.0)  # 圆角的直径
  $e = [single]($size - $d)
  $z = [single]0
  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $path.AddArc($z, $z, $d, $d, [single]180, [single]90)
  $path.AddArc($e, $z, $d, $d, [single]270, [single]90)
  $path.AddArc($e, $e, $d, $d, [single]0, [single]90)
  $path.AddArc($z, $e, $d, $d, [single]90, [single]90)
  $path.CloseFigure()
  $g.FillPath($brush, $path)
  # 两位数字要挤在 16 像素里，字号按位数调；“满”这样的汉字用雅黑
  $family = if ($badge -match '^[\x20-\x7E]*$') { "Segoe UI" } else { "Microsoft YaHei" }
  $px = if ($badge.Length -ge 2) { $size * 0.66 } else { $size * 0.8 }
  $font = New-Object System.Drawing.Font($family, [single]$px, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
  $fmt = New-Object System.Drawing.StringFormat
  $fmt.Alignment = [System.Drawing.StringAlignment]::Center
  $fmt.LineAlignment = [System.Drawing.StringAlignment]::Center
  $rect = New-Object System.Drawing.RectangleF(0, [single]($size * 0.04), $size, $size)
  $g.DrawString($badge, $font, $ink, $rect, $fmt)
  $f = New-Frame $bmp
  $fmt.Dispose(); $font.Dispose(); $path.Dispose(); $ink.Dispose(); $brush.Dispose(); $g.Dispose(); $bmp.Dispose()
  return $f
}

# 红绿灯 + 数字的数字：写在图标右边那块（从 $left 像素到右边），竖着居中；两位数在 16 像素里挤不下时横向压一点。
# 透明底上写字，字的颜色跟任务栏：深色任务栏白字、浅色黑字
function Add-TrayText($g, [string]$text, [bool]$dark, [double]$left) {
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
  $cjk = -not ($text -match '^[\x20-\x7E]*$')  # “满”用雅黑
  $family = if ($cjk) { "Microsoft YaHei" } else { "Segoe UI" }
  $em = if ($text.Length -ge 2) { $IconPx * 0.68 } else { $IconPx * 0.78 }
  $style = [System.Drawing.FontStyle]::Bold
  $font = New-Object System.Drawing.Font($family, [single]$em, $style, [System.Drawing.GraphicsUnit]::Pixel)
  $color = if ($dark) { [System.Drawing.Color]::FromArgb(240, 255, 255, 255) } else { [System.Drawing.Color]::FromArgb(230, 0, 0, 0) }
  $brush = New-Object System.Drawing.SolidBrush($color)
  $fmt = New-Object System.Drawing.StringFormat([System.Drawing.StringFormat]::GenericTypographic)  # 不留左右空白，宽度算得准
  $size = $g.MeasureString($text, $font, (New-Object System.Drawing.PointF(0, 0)), $fmt)
  $area = $IconPx - $left
  $sx = [Math]::Min(1.0, $area / [Math]::Max(1.0, $size.Width))
  # 竖着按字的样子居中：数字的中线在基线上方约 0.35 个字号（汉字约 0.38），基线在行顶往下 ascent 处
  $ff = $font.FontFamily
  $asc = $em * $ff.GetCellAscent($style) / $ff.GetEmHeight($style)
  $mid = if ($cjk) { 0.38 } else { 0.35 }
  $top = $IconPx / 2 + $mid * $em - $asc
  $g.TranslateTransform([single]($left + ($area - $size.Width * $sx) / 2), [single]$top)
  $g.ScaleTransform([single]$sx, [single]1)
  $g.DrawString($text, $font, $brush, [single]0, [single]0, $fmt)
  $g.ResetTransform()
  $fmt.Dispose(); $brush.Dispose(); $font.Dispose()
}

# cam_tray.py 画好的一帧（PNG）：长边等于托盘图标大小，摆到正中（红绿灯竖着、两根小条横着）。
# 红绿灯 + 数字：PNG 是整格、只有左边三颗小灯，$text（5 小时用了百分之几）在这里写到右边，从 $textAt 像素开始
function New-PngFrame([string]$file, [string]$text = "", [bool]$dark = $true, [double]$textAt = 0) {
  $bytes = [System.IO.File]::ReadAllBytes($file)
  $ms = New-Object System.IO.MemoryStream(, $bytes)
  $img = [System.Drawing.Image]::FromStream($ms)
  $bmp = New-Object System.Drawing.Bitmap($IconPx, $IconPx, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.Clear([System.Drawing.Color]::Transparent)
  $x = [Math]::Floor(($IconPx - $img.Width) / 2)
  $y = [Math]::Floor(($IconPx - $img.Height) / 2)
  $g.DrawImage($img, [int]$x, [int]$y, $img.Width, $img.Height)  # 给定宽高：按像素画，不按 PNG 里记的 dpi 缩放
  if ($text) { Add-TrayText $g $text $dark $textAt }
  $f = New-Frame $bmp
  $g.Dispose(); $bmp.Dispose(); $img.Dispose(); $ms.Dispose()
  return $f
}

function Remove-Frames($frames) {
  foreach ($f in @($frames)) {
    if (-not $f) { continue }
    $f.icon.Dispose()
    [void][CamTray.Native]::DestroyIcon($f.handle)
  }
}

# 动画：和 macOS 菜单栏一样 0.25 秒一帧、8 帧两秒一圈。工作中（pulse）亮半秒暗半秒，等你确认（blink）快一倍
$script:frames = @()
$script:frameAt = 0
$script:iconKey = ""
function Set-TrayIcon($spec) {
  $key = $spec | ConvertTo-Json -Compress -Depth 4
  if ($key -eq $script:iconKey) { return }
  $new = @()
  if ($spec.style -ne "badge" -and @($spec.frames).Count) {
    $made = @{}
    foreach ($file in @($spec.frames)) {
      if (-not $made.ContainsKey($file)) { $made[$file] = New-PngFrame $file ([string]$spec.text) ([bool]$spec.dark) ([double]$spec.text_at) }
    }
    $new = @(@($spec.frames) | ForEach-Object { $made[$_] })
    $own = @($made.Values)
  } else {
    $lit = New-BadgeFrame ([string]$spec.level) ([string]$spec.text)
    $own = @($lit)
    if ($spec.kind -eq "pulse") {
      $dim = New-BadgeFrame ([string]$spec.level) ([string]$spec.text) 0.35 1.0
      $own += $dim
      $new = @($lit, $lit, $dim, $dim, $lit, $lit, $dim, $dim)
    } elseif ($spec.kind -eq "blink") {
      $dim = New-BadgeFrame ([string]$spec.level) ([string]$spec.text) 1.0 0.25
      $own += $dim
      $new = @($lit, $dim, $lit, $dim, $lit, $dim, $lit, $dim)
    } else {
      $new = @($lit)
    }
  }
  $old = $script:ownFrames
  $script:frames = $new
  $script:ownFrames = $own
  $script:frameAt = 0
  $script:notify.Icon = $new[0].icon
  Remove-Frames $old  # 新图标已经换上了，再释放旧的
  $script:iconKey = $key
  if ($new.Count -gt 1) {
    $script:anim.Interval = [Math]::Max(100, [int]$spec.ms)
    $script:anim.Start()
  } else {
    $script:anim.Stop()
  }
}
$script:ownFrames = @()

# ---- 菜单
$script:pending = $null
$script:menuFont = New-Object System.Drawing.Font("Microsoft YaHei UI", 9)

# 系统的应用模式是不是深色（设置 → 个性化 → 颜色）：菜单跟着它
function Test-DarkMenu {
  try {
    $v = Get-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize" -Name AppsUseLightTheme -ErrorAction Stop
    return ($v.AppsUseLightTheme -eq 0)
  } catch { return $false }
}
$script:darkMenu = Test-DarkMenu
$script:renderer = New-Object CamTray.MenuRenderer($script:darkMenu)

function Set-MenuLook($dd) {
  $dd.Renderer = $script:renderer
  $dd.Font = $script:menuFont
  $dd.ShowItemToolTips = $true
  $dd.ShowCheckMargin = $true
  $dd.ShowImageMargin = $false
  $dd.Padding = New-Object System.Windows.Forms.Padding(0, 4, 0, 4)
}

# 颜色写成浅色、深色两个（cam_tray.py 给的 color / colorDark），按菜单现在的深浅取一个
function Get-MenuColor($light, $dark) {
  $c = if ($script:darkMenu -and $dark) { $dark } else { $light }
  return [System.Drawing.ColorTranslator]::FromHtml([string]$c)
}

function Add-MenuItems($coll, $items) {
  foreach ($it in @($items)) {
    if (-not $it) { continue }
    if ($it.sep) { [void]$coll.Add((New-Object System.Windows.Forms.ToolStripSeparator)); continue }
    # 菜单里的 & 是快捷键标记，账号名、邮箱里的 & 要写成 && 才显示得出来
    $txt = ([string]$it.text).Replace("&", "&&")
    if ($null -ne $it.bar) {
      # 账号行：名字下面画 5 小时的额度条，行高一点给条留地方；右边对齐写额度
      $mi = New-Object CamTray.BarItem($txt)
      $mi.Bar = [double]$it.bar
      $mi.BarColor = Get-MenuColor $it.barColor $it.barColorDark
      $mi.Padding = New-Object System.Windows.Forms.Padding(0, 4, 0, 11)
    } else {
      $mi = New-Object System.Windows.Forms.ToolStripMenuItem($txt)
      $mi.Padding = New-Object System.Windows.Forms.Padding(0, 3, 0, 3)
    }
    if ($it.right) { $mi.ShortcutKeyDisplayString = [string]$it.right }
    if ($it.tip) { $mi.ToolTipText = [string]$it.tip }
    if ($it.checked) { $mi.Checked = $true }
    if ($it.color) { $mi.ForeColor = Get-MenuColor $it.color $it.colorDark }
    if ($it.children) {
      # 有子菜单的那一行点了只展开子菜单，和 macOS 一样；切换在子菜单里的“切换到这个账号”
      Add-MenuItems $mi.DropDownItems $it.children
      Set-MenuLook $mi.DropDown
      [CamTray.Look]::Round($mi.DropDown)
    } elseif ($it.run) {
      $mi.Tag = $it
      $mi.Add_Click({ try { Invoke-MenuItem $this.Tag } catch { Add-Note "托盘小工具出错了" $_.Exception.Message } })
    } elseif ($it.dim) {
      $mi.Enabled = $false
    } else {
      $mi.AccessibleRole = [System.Windows.Forms.AccessibleRole]::StaticText  # 说明行：点了不做事，鼠标移上去也不高亮
    }
    [void]$coll.Add($mi)
  }
}

function Invoke-MenuItem($it) {
  Start-TrayPy (@("do") + @($it.run | ForEach-Object { [string]$_ })) "do" ([bool]$it.refresh)
}

# ---- 卡片小面板：和 macOS 菜单栏弹窗同一张页面（本机 cam serve 的 /mini），用 Edge / Chrome 的应用模式开成一个没有地址栏的小窗，
# 贴在屏幕右下角（托盘上方）；再点一下托盘图标就关掉。没有 Edge 也没有 Chrome 就用默认浏览器打开。
$script:mini = $null
$CardTitle = "Claude 账号卡片"  # mini.html 的 <title>，应用模式的窗口标题就是它，按它找窗口（面板和桌面版叫“Claude 账号”，不能混）

function Find-Browser {
  # 先 Edge 后 Chrome；哪个目录变量是空的（比如 32 位系统没有 ProgramFiles(x86)）就跳过，Join-Path 不接受空路径
  $bases = @(${env:ProgramFiles(x86)}, $env:ProgramFiles, $env:LOCALAPPDATA) | Where-Object { $_ }
  foreach ($rel in @("Microsoft\Edge\Application\msedge.exe", "Google\Chrome\Application\chrome.exe")) {
    foreach ($b in $bases) {
      $c = Join-Path $b $rel
      if (Test-Path -LiteralPath $c) { return $c }
    }
  }
  return $null
}

function Toggle-Card {
  if (-not $script:mini) {  # 没有卡片页（在跑的是桌面版的服务、或者起不来）：左键还是弹菜单
    if ($showMenu) { try { [void]$showMenu.Invoke($script:notify, $null) } catch { } }
    return
  }
  $h = [CamTray.Native]::FindWindow($null, $CardTitle)
  if ($h -ne [IntPtr]::Zero) { [void][CamTray.Native]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero); return }  # WM_CLOSE
  $browser = Find-Browser
  $url = [string]$script:mini.url
  if (-not $browser) { Start-Process $url; return }
  $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
  $w = [int]$script:mini.w + 16   # 窗口边框
  $hh = [int]$script:mini.h + 40  # 标题栏
  $x = $wa.Right - $w - 12
  $y = $wa.Bottom - $hh - 12
  # 单独的浏览器配置目录：浏览器已经开着时，新开的窗口会交给老进程，尺寸和位置参数就不认了；另起一个进程才听话
  $cardDir = Join-Path $env:LOCALAPPDATA "cam\card-browser"
  Start-Process -FilePath $browser -ArgumentList @("--app=$url", "--user-data-dir=`"$cardDir`"", "--no-first-run", "--no-default-browser-check",
    "--window-size=$w,$hh", "--window-position=$x,$y")
}

# 图标和鼠标停上去的提示：菜单和 light 的结果都有
function Update-Icon($res) {
  $tip = [string]$res.tip
  if ($tip.Length -gt 63) { $tip = $tip.Substring(0, 62) + "…" }  # 托盘提示最多 63 个字
  $script:notify.Text = $tip
  $spec = $res.icon
  if (-not $spec) { $spec = @{ style = "badge"; kind = ""; ms = 250; text = [string]$res.badge; level = [string]$res.level } }  # 出错时只有 badge
  Set-TrayIcon $spec
  $script:watch = [bool]$res.watch
}

function Update-Tray($res) {
  $script:mini = $res.mini
  Update-Icon $res
  # 菜单开着的时候不换内容，关上以后再换
  if ($script:menu.Visible) { $script:pending = $res.items; return }
  Set-Menu $res.items
}

function Set-Menu($items) {
  # 系统换了深色 / 浅色：换一套颜色
  $d = Test-DarkMenu
  if ($d -ne $script:darkMenu) {
    $script:darkMenu = $d
    $script:renderer = New-Object CamTray.MenuRenderer($d)
    Set-MenuLook $script:menu
  }
  # 旧的菜单项释放掉（每分钟换一批，不释放就交给垃圾回收慢慢清）；只在菜单没打开时调用
  $old = @($script:menu.Items | ForEach-Object { $_ })
  $script:menu.Items.Clear()
  foreach ($o in $old) { $o.Dispose() }
  Add-MenuItems $script:menu.Items $items
  [void]$script:menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))
  $again = New-Object System.Windows.Forms.ToolStripMenuItem("重启托盘小工具")
  $again.ToolTipText = "退出再重新打开：显示不对、点了没反应时用"
  $again.Padding = New-Object System.Windows.Forms.Padding(0, 3, 0, 3)
  $again.Add_Click({ Invoke-AfterMenu "restart" })
  [void]$script:menu.Items.Add($again)
  $quit = New-Object System.Windows.Forms.ToolStripMenuItem("退出托盘小工具")
  $quit.ToolTipText = "只退出这一次，下次登录 Windows 时还会自动运行"
  $quit.Padding = New-Object System.Windows.Forms.Padding(0, 3, 0, 3)
  $quit.Add_Click({ Invoke-AfterMenu "quit" })
  [void]$script:menu.Items.Add($quit)
}

function Stop-Tray {
  foreach ($t in @($script:poll, $script:tick, $script:anim, $script:lightTick, $script:afterMenu)) { if ($t) { $t.Stop() } }
  $script:notify.Visible = $false
  $script:notify.Dispose()
  [System.Windows.Forms.Application]::Exit()
}

# 自己退出、再起一个新的：菜单里的“重启托盘小工具”，还有这个文件更新以后。先放开“只许一个在运行”的锁，新的那个才起得来
function Restart-Tray {
  $Mutex.ReleaseMutex()
  $Mutex.Dispose()
  $conhost = Join-Path $env:WINDIR "System32\conhost.exe"
  $ps = (Get-Process -Id $PID).Path
  Start-Process -FilePath $conhost -WindowStyle Hidden -ArgumentList @(
    "--headless", (ConvertTo-Arg $ps), "-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File", (ConvertTo-Arg $Self))
  Stop-Tray
}

# 这个文件被 cam update 换成新版本后，自己重启一次。重启了返回 $true
function Restart-IfUpdated {
  if ($script:menu -and $script:menu.Visible) { return $false }  # 菜单开着：下一分钟再说，不在人家点菜单的时候退出
  # 文件暂时不在（cam update 正在替换）就当没变；Get-Item 找不到文件默认只是提醒、进不了 catch，要加 -ErrorAction Stop
  try { $t = (Get-Item -LiteralPath $Self -ErrorAction Stop).LastWriteTimeUtc } catch { return $false }
  if ($t -eq $SelfTime) { return $false }
  Restart-Tray
  return $true
}

# ---- 菜单收起以后再做的事：换上新菜单、退出、重启。
# 不能在菜单项的 Click、菜单的 Closed 里当场做：WinForms 接着还要收起子菜单，那时旧菜单项（连同它的子菜单）已经被释放了，
# 就会弹“无法访问已释放的对象（ToolStripDropDownMenu）”的错误框。所以记下要做什么，等这一轮点击处理完（50 毫秒后）再做
$script:afterAction = $null
$script:afterMenu = New-Object System.Windows.Forms.Timer
$script:afterMenu.Interval = 50
$script:afterMenu.Add_Tick({
  $script:afterMenu.Stop()
  $a = $script:afterAction; $script:afterAction = $null
  if ($a -eq "quit") { Stop-Tray; return }
  if ($a -eq "restart") {
    try { Restart-Tray } catch { Add-Note "托盘小工具没能重启" $_.Exception.Message }
    return
  }
  if ($script:menu.Visible) { return }  # 又打开了：等它下次收起
  if ($script:pending) {
    $items = $script:pending; $script:pending = $null
    try { Set-Menu $items } catch { Add-Note "托盘小工具出错了" $_.Exception.Message }
  }
})
function Invoke-AfterMenu([string]$action) {
  if ($action) { $script:afterAction = $action }
  $script:afterMenu.Stop()
  $script:afterMenu.Start()
}

# ---- 托盘图标
$script:menu = New-Object System.Windows.Forms.ContextMenuStrip
Set-MenuLook $script:menu
[CamTray.Look]::Round($script:menu)
$script:menu.Add_Closed({ if ($script:pending) { Invoke-AfterMenu "" } })
Set-Menu @(@{ text = "正在读取账号…"; dim = $true })

$script:notify = New-Object System.Windows.Forms.NotifyIcon
$script:notify.ContextMenuStrip = $script:menu
$script:notify.Text = "Claude 账号"
$script:anim = New-Object System.Windows.Forms.Timer
$script:anim.Interval = 250
$script:anim.Add_Tick({
  try {
    if ($script:frames.Count -gt 1) {
      $script:frameAt = ($script:frameAt + 1) % $script:frames.Count
      $script:notify.Icon = $script:frames[$script:frameAt].icon
    }
  } catch { $script:anim.Stop(); Add-Note "托盘小工具出错了" $_.Exception.Message }
})
Set-TrayIcon @{ style = "badge"; kind = ""; ms = 250; text = "C"; level = "gray" }
$script:notify.Visible = $true
# 左键开 / 关卡片小窗（没有卡片页时弹菜单：借用 NotifyIcon 内部弹菜单的方法），右键菜单由 NotifyIcon 自己弹
$showMenu = [System.Windows.Forms.NotifyIcon].GetMethod("ShowContextMenu", [System.Reflection.BindingFlags]"Instance,NonPublic")
$script:notify.Add_MouseUp({
  param($s, $e)
  if ($e.Button -eq [System.Windows.Forms.MouseButtons]::Left) { try { Toggle-Card } catch { Add-Note "托盘小工具出错了" $_.Exception.Message } }
})

# 事件处理里冒出来的异常会让 WinForms 弹“未处理的异常”对话框，点“退出”托盘就没了，所以下面的处理都接住变成通知。
# 每半秒看一眼后台的 Python 跑完没有、有没有要弹的通知
$script:poll = New-Object System.Windows.Forms.Timer
$script:poll.Interval = 500
$script:poll.Add_Tick({
  try {
    foreach ($j in @($script:jobs)) {
      if (-not $j.proc.HasExited) {
        if (((Get-Date) - $j.started).TotalMinutes -gt 15) {
          # 卡住了。py 启动器下面还有 python.exe，cam.py 又会再起子进程，要连整棵进程树一起结束：
          # 只结束最上面那个的话，管道一直有人握着，这个任务永远算不上完成，菜单也就不再刷新
          & (Join-Path $env:WINDIR "System32\taskkill.exe") /PID $j.proc.Id /T /F 2>&1 | Out-Null
        }
        continue
      }
      if (-not ($j.out.IsCompleted -and $j.err.IsCompleted)) { continue }
      $script:jobs = @($script:jobs | Where-Object { $_ -ne $j })
      Complete-Job $j
      $j.proc.Dispose()
    }
    Show-NextNote
  } catch { Add-Note "托盘小工具出错了" $_.Exception.Message }
})
$script:poll.Start()

$script:tick = New-Object System.Windows.Forms.Timer
$script:tick.Interval = 60000
$script:tick.Add_Tick({
  try { if (-not (Restart-IfUpdated)) { Request-Menu } } catch { Add-Note "托盘小工具出错了" $_.Exception.Message }
})
$script:tick.Start()

$script:lightTick = New-Object System.Windows.Forms.Timer
$script:lightTick.Interval = 20000
$script:lightTick.Add_Tick({ try { Request-Light } catch { Add-Note "托盘小工具出错了" $_.Exception.Message } })
$script:lightTick.Start()

Request-Menu
[System.Windows.Forms.Application]::Run()
Remove-Frames $script:ownFrames
