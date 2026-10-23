# cam 的 Windows 托盘小工具：任务栏右下角的图标上写着当前 Claude Code 账号 5 小时额度用了百分之几，
# 底色跟着额度变（70% 以下绿、70–89% 黄、90% 以上红）。左键或右键点开能看到所有账号的额度，点一下切换。
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
'@

$created = $false
$Mutex = New-Object System.Threading.Mutex($true, "Local\cam-tray", [ref]$created)
if (-not $created) { exit 0 }

# 不是用 conhost --headless 启动的话，至少把自己的控制台窗口藏起来
[void][CamTray.Native]::ShowWindow([CamTray.Native]::GetConsoleWindow(), 0)
[void][CamTray.Native]::SetProcessDPIAware()  # 高分屏上图标按实际大小画，不发虚
[System.Windows.Forms.Application]::EnableVisualStyles()

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
  Start-TrayPy @("menu") "menu" $false
}

function Complete-Job($j) {
  try {
    $res = $null
    # cam_tray.py 最后一行是 JSON
    $rows = @(($j.out.Result -split "`r?`n") | Where-Object { $_.Trim() })
    if ($rows.Count) { try { $res = $rows[-1] | ConvertFrom-Json } catch { $res = $null } }
    if (-not $res) {
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

# ---- 图标：圆角方块，写着百分比，底色跟着额度变
$script:iconKey = ""
$script:iconHandle = [IntPtr]::Zero
function Set-TrayIcon([string]$level, [string]$badge) {
  $key = "$level|$badge"
  if ($key -eq $script:iconKey) { return }
  $size = [System.Windows.Forms.SystemInformation]::SmallIconSize.Width
  $hex = switch ($level) { "green" { "#1F8A4C" } "yellow" { "#B87A08" } "red" { "#BF3B33" } default { "#6E6E73" } }
  $bmp = New-Object System.Drawing.Bitmap($size, $size)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
  $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
  $brush = New-Object System.Drawing.SolidBrush([System.Drawing.ColorTranslator]::FromHtml($hex))
  $r = [Math]::Max(2, [int]($size / 4))
  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $path.AddArc(0, 0, 2 * $r, 2 * $r, 180, 90)
  $path.AddArc($size - 1 - 2 * $r, 0, 2 * $r, 2 * $r, 270, 90)
  $path.AddArc($size - 1 - 2 * $r, $size - 1 - 2 * $r, 2 * $r, 2 * $r, 0, 90)
  $path.AddArc(0, $size - 1 - 2 * $r, 2 * $r, 2 * $r, 90, 90)
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
  $g.DrawString($badge, $font, [System.Drawing.Brushes]::White, $rect, $fmt)
  $h = $bmp.GetHicon()
  $script:notify.Icon = [System.Drawing.Icon]::FromHandle($h)
  # 图标句柄要自己释放，不然每分钟漏一个，几天后托盘就画不出来了
  if ($script:iconHandle -ne [IntPtr]::Zero) { [void][CamTray.Native]::DestroyIcon($script:iconHandle) }
  $script:iconHandle = $h
  $script:iconKey = $key
  $fmt.Dispose(); $font.Dispose(); $path.Dispose(); $brush.Dispose(); $g.Dispose(); $bmp.Dispose()
}

# ---- 菜单
$script:pending = $null
function Add-MenuItems($coll, $items) {
  foreach ($it in @($items)) {
    if (-not $it) { continue }
    if ($it.sep) { [void]$coll.Add((New-Object System.Windows.Forms.ToolStripSeparator)); continue }
    # 菜单里的 & 是快捷键标记，账号名、邮箱里的 & 要写成 && 才显示得出来
    $mi = New-Object System.Windows.Forms.ToolStripMenuItem(([string]$it.text).Replace("&", "&&"))
    if ($it.checked) { $mi.Checked = $true }
    if ($it.color) { $mi.ForeColor = [System.Drawing.ColorTranslator]::FromHtml([string]$it.color) }
    if ($it.children) {
      # 有子菜单的那一行点了只展开子菜单，和 macOS 一样；切换在子菜单里的“切换到这个账号”
      Add-MenuItems $mi.DropDownItems $it.children
    } elseif ($it.run) {
      $mi.Tag = $it
      $mi.Add_Click({ try { Invoke-MenuItem $this.Tag } catch { Add-Note "托盘小工具出错了" $_.Exception.Message } })
    } elseif ($it.dim) {
      $mi.Enabled = $false
    }
    [void]$coll.Add($mi)
  }
}

function Invoke-MenuItem($it) {
  Start-TrayPy (@("do") + @($it.run | ForEach-Object { [string]$_ })) "do" ([bool]$it.refresh)
}

function Update-Tray($res) {
  $tip = [string]$res.tip
  if ($tip.Length -gt 63) { $tip = $tip.Substring(0, 62) + "…" }  # 托盘提示最多 63 个字
  $script:notify.Text = $tip
  Set-TrayIcon ([string]$res.level) ([string]$res.badge)
  # 菜单开着的时候不换内容，关上以后再换
  if ($script:menu.Visible) { $script:pending = $res.items; return }
  Set-Menu $res.items
}

function Set-Menu($items) {
  # 旧的菜单项释放掉（每分钟换一批，不释放就交给垃圾回收慢慢清）；只在菜单没打开时调用
  $old = @($script:menu.Items | ForEach-Object { $_ })
  $script:menu.Items.Clear()
  foreach ($o in $old) { $o.Dispose() }
  Add-MenuItems $script:menu.Items $items
  [void]$script:menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))
  $quit = New-Object System.Windows.Forms.ToolStripMenuItem("退出托盘小工具（下次登录 Windows 时自动运行）")
  $quit.Add_Click({ Stop-Tray })
  [void]$script:menu.Items.Add($quit)
}

function Stop-Tray {
  foreach ($t in @($script:poll, $script:tick)) { if ($t) { $t.Stop() } }
  $script:notify.Visible = $false
  $script:notify.Dispose()
  [System.Windows.Forms.Application]::Exit()
}

# 这个文件被 cam update 换成新版本后，自己重启一次。重启了返回 $true
function Restart-IfUpdated {
  # 文件暂时不在（cam update 正在替换）就当没变；Get-Item 找不到文件默认只是提醒、进不了 catch，要加 -ErrorAction Stop
  try { $t = (Get-Item -LiteralPath $Self -ErrorAction Stop).LastWriteTimeUtc } catch { return $false }
  if ($t -eq $SelfTime) { return $false }
  $Mutex.ReleaseMutex()
  $Mutex.Dispose()
  $conhost = Join-Path $env:WINDIR "System32\conhost.exe"
  $ps = (Get-Process -Id $PID).Path
  Start-Process -FilePath $conhost -WindowStyle Hidden -ArgumentList @(
    "--headless", (ConvertTo-Arg $ps), "-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File", (ConvertTo-Arg $Self))
  Stop-Tray
  return $true
}

# ---- 托盘图标
$script:menu = New-Object System.Windows.Forms.ContextMenuStrip
$script:menu.Font = New-Object System.Drawing.Font("Microsoft YaHei UI", 9)
$script:menu.ShowCheckMargin = $true
$script:menu.ShowImageMargin = $false
$script:menu.Add_Closed({
  if ($script:pending) {
    $items = $script:pending; $script:pending = $null
    try { Set-Menu $items } catch { Add-Note "托盘小工具出错了" $_.Exception.Message }
  }
})
Set-Menu @(@{ text = "正在读取账号…"; dim = $true })

$script:notify = New-Object System.Windows.Forms.NotifyIcon
$script:notify.ContextMenuStrip = $script:menu
$script:notify.Text = "Claude 账号"
Set-TrayIcon "gray" "C"
$script:notify.Visible = $true
# 左键也弹出菜单（NotifyIcon 只在右键时自己弹，左键借用它内部弹菜单的方法）
$showMenu = [System.Windows.Forms.NotifyIcon].GetMethod("ShowContextMenu", [System.Reflection.BindingFlags]"Instance,NonPublic")
$script:notify.Add_MouseUp({
  param($s, $e)
  if ($e.Button -eq [System.Windows.Forms.MouseButtons]::Left -and $showMenu) { try { [void]$showMenu.Invoke($script:notify, $null) } catch { } }
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

Request-Menu
[System.Windows.Forms.Application]::Run()
if ($script:iconHandle -ne [IntPtr]::Zero) { [void][CamTray.Native]::DestroyIcon($script:iconHandle) }
