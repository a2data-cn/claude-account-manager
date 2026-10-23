# 一键安装 cam（Windows）。也可以直接双击“安装.cmd”。
# 用法：powershell -ExecutionPolicy Bypass -File install.ps1 [-Yes] [-SkipVerify] [-Uninstall]
#   -Yes         不提问，全部按默认选择：没有 Python 时用 winget 装、装托盘小工具；Claude Code 不装
#   -SkipVerify  跳过 SHA256SUMS 文件校验（自己改过代码时用）
#   -Uninstall   卸载：把 scripts 目录从用户 PATH 里去掉，删掉托盘小工具
# 也认 --yes / --skip-verify / --uninstall 的写法。可以重复运行，装好的部分会跳过；
# 账号数据（%USERPROFILE%\.claude-accounts）和 Claude Code 配置（%USERPROFILE%\.claude）都不会动。
param([switch]$Yes, [switch]$SkipVerify, [switch]$Uninstall)

# python 等外部命令会往 stderr 写正常输出，Windows PowerShell 在 Stop 模式下会把它当成错误中断，所以只看退出码
$ErrorActionPreference = "Continue"
foreach ($a in $args) {
  if ($a -eq "--yes" -or $a -eq "-y") { $Yes = $true }
  elseif ($a -eq "--skip-verify") { $SkipVerify = $true }
  elseif ($a -eq "--uninstall") { $Uninstall = $true }
  else { Write-Host "不认识的参数：$a"; exit 2 }
}

$Root = $PSScriptRoot
$Scripts = Join-Path $Root "scripts"
$CamPy = Join-Path $Scripts "cam.py"
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
$env:PYTHONIOENCODING = "utf-8"  # 读 cam doctor 的输出时按 UTF-8 解码，中文不乱码

function Step($m) { Write-Host ""; Write-Host "▶ $m" }
function Ok($m) { Write-Host "  ✓ $m" -ForegroundColor Green }
function Note($m) { Write-Host "  · $m" }
function Warn($m) { Write-Host "  ! $m" -ForegroundColor Yellow }
function Fail($m) { Write-Host ""; Write-Host "✗ $m" -ForegroundColor Red; exit 1 }

# 直接回车用默认值；-Yes 时不问，直接用默认值
function Ask($question, [bool]$default) {
  if ($Yes) {
    Write-Host "  $question → $(if ($default) { '是' } else { '否' })（-Yes，按默认）"
    return $default
  }
  $hint = if ($default) { "[Y/n]" } else { "[y/N]" }
  $ans = Read-Host "  $question $hint"
  if (-not $ans) { return $default }
  return @("y", "yes", "是") -contains $ans.Trim().ToLower()
}

# 用户 PATH 直接读写注册表，保留 %USERPROFILE% 这类写法不被展开
function Get-UserPath {
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment")
  if (-not $key) { return "" }
  try { return [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }
  finally { $key.Close() }
}
function Set-UserPath([string]$value) {
  $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Environment")
  try { $key.SetValue("Path", $value, [Microsoft.Win32.RegistryValueKind]::ExpandString) } finally { $key.Close() }
  # 随便设一个用户环境变量再删掉：系统会通知开着的程序“环境变量变了”，之后新开的终端就找得到 cam
  [Environment]::SetEnvironmentVariable("CAM_INSTALL_PING", "1", "User")
  [Environment]::SetEnvironmentVariable("CAM_INSTALL_PING", $null, "User")
}
function Expand-Dir([string]$p) { return [Environment]::ExpandEnvironmentVariables($p).TrimEnd("\") }
function Test-ThisCam([string]$p) { return (Expand-Dir $p) -ieq $Scripts.TrimEnd("\") }
# 以前从别的文件夹装过的另一份 cam：重新下载新版本后再运行安装，就换成新目录
function Test-OtherCam([string]$p) {
  $d = Expand-Dir $p
  return (-not (Test-ThisCam $p)) -and (Test-Path -LiteralPath (Join-Path $d "cam.cmd")) -and (Test-Path -LiteralPath (Join-Path $d "cam.py"))
}

# 托盘小工具：“启动”文件夹里放一个快捷方式，登录 Windows 后用 conhost --headless 在后台运行 Windows PowerShell，不弹黑窗口
$TrayPs1 = Join-Path $Root "extras\windows-tray\cam-tray.ps1"
$TrayLink = Join-Path ([Environment]::GetFolderPath("Startup")) "Claude 账号托盘.lnk"
$Conhost = Join-Path $env:WINDIR "System32\conhost.exe"
$WinPS = Join-Path $env:WINDIR "System32\WindowsPowerShell\v1.0\powershell.exe"
$TrayArgs = "--headless `"$WinPS`" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$TrayPs1`""
function Get-TrayProcess {
  @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' OR Name = 'pwsh.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -like "*cam-tray.ps1*" })
}
function Stop-TrayProcess {
  foreach ($p in Get-TrayProcess) {
    Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
    # 等它真的退出、放开“只运行一个”的锁，再起新的
    Wait-Process -Id $p.ProcessId -Timeout 5 -ErrorAction SilentlyContinue
  }
}
function Start-Tray { Start-Process -FilePath $Conhost -ArgumentList $TrayArgs -WindowStyle Hidden }

if ($Uninstall) {
  Write-Host "卸载 cam（账号数据和 Claude Code 配置都不动）"
  Step "cam 命令"
  $entries = @((Get-UserPath) -split ";" | Where-Object { $_ })
  $keep = @($entries | Where-Object { -not (Test-ThisCam $_) })
  if ($keep.Count -lt $entries.Count) {
    Set-UserPath ($keep -join ";")
    Ok "已把 $Scripts 从用户 PATH 里去掉（新开的终端生效）"
  } else {
    Note "用户 PATH 里没有 $Scripts"
  }
  Step "托盘小工具"
  $running = Get-TrayProcess
  Stop-TrayProcess
  if (Test-Path -LiteralPath $TrayLink) {
    Remove-Item -LiteralPath $TrayLink -Force
    Ok "已删掉开机自动运行的快捷方式 $TrayLink"
  } else {
    Note "没装托盘小工具"
  }
  if ($running.Count) { Ok "已关掉正在运行的托盘小工具（任务栏上的图标鼠标移过去就会消失）" }
  Write-Host ""
  Write-Host "✓ 卸载完成。代码目录 $Root 和账号数据 $env:USERPROFILE\.claude-accounts 都还在；数据目录里有各账号的登录凭据，确定不要了再自己删。"
  Write-Host "  装过桌面版的话，在`“设置 → 应用`”里卸载。"
  exit 0
}

Write-Host "安装 cam（Claude 多账号管理）：$Root"

# ---- 1. Python：后面的校验和 cam 本身都要用
Step "检查 Python"
# 依次试 py -3、python、python3。Microsoft Store 的 python 占位程序跑不起来，要真的运行一下才知道能不能用
$PyExe = $null
$PyArgs = @()
function Find-Python {
  foreach ($cand in "py -3", "python", "python3") {
    $parts = $cand -split " "
    $cmd = Get-Command $parts[0] -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cmd) { continue }
    $extra = @($parts | Select-Object -Skip 1)
    & $cmd.Source @extra -c "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)" 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) { $script:PyExe = $cmd.Source; $script:PyArgs = $extra; return }
  }
}
Find-Python
if (-not $PyExe) {
  $winget = Get-Command winget -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($winget) {
    Note "没找到 Python 3.8 以上，cam 要用它。可以用 Windows 自带的 winget 装 Python 官方的 3.13 版（只装给当前用户，不用管理员权限，下载大约 30 MB）"
    if (Ask "现在装 Python 吗？" $true) {
      & $winget.Source install --id Python.Python.3.13 -e --source winget --accept-package-agreements --accept-source-agreements `
        --override "/quiet InstallAllUsers=0 PrependPath=1 Include_launcher=1 InstallLauncherAllUsers=0 Include_test=0"
      # 新装的 Python 加进了用户 PATH，这个窗口里还是旧的：从注册表重新读一遍
      $env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User")
      Find-Python
      if (-not $PyExe) { Warn "winget 没装成，见上面的输出" }
    }
  }
}
if (-not $PyExe) {
  Fail "没找到 Python 3.8 以上。到 https://www.python.org/downloads/ 下载安装（安装时勾上`“Add python.exe to PATH`”），装好后重新运行"
}
Ok "$((& $PyExe @PyArgs -V 2>&1) -join ' ')（$PyExe $($PyArgs -join ' ')）"

# ---- 2. 校验文件：安装任何东西之前先确认代码完整、没被改过
Step "校验文件（SHA256SUMS）"
if ($SkipVerify) {
  Warn "按 -SkipVerify 跳过了校验"
} else {
  & $PyExe @PyArgs $CamPy verify
  if ($LASTEXITCODE -ne 0) {
    Fail "文件校验没通过，安装停止，什么都没改。重新下载完整的代码再装；上面列出的文件如果是你自己改的，加 -SkipVerify 跳过"
  }
}

# ---- 3. cam 命令：scripts 目录加进用户 PATH（里面的 cam.cmd 就是 cam 命令）
Step "安装 cam 命令（把 scripts 目录加进用户 PATH）"
$entries = @((Get-UserPath) -split ";" | Where-Object { $_ })
$others = @($entries | Where-Object { Test-OtherCam $_ })
$has = @($entries | Where-Object { Test-ThisCam $_ }).Count -gt 0
if ($has -and $others.Count -eq 0) {
  Ok "已经装好了：$Scripts 在用户 PATH 里"
} else {
  $new = @($entries | Where-Object { -not (Test-ThisCam $_) -and -not (Test-OtherCam $_) }) + $Scripts
  Set-UserPath ($new -join ";")
  foreach ($o in $others) { Note "去掉了以前装的另一份：$o" }
  Ok "已把 $Scripts 加进用户 PATH（新开的终端生效）"
}
if (-not @($env:Path -split ";" | Where-Object { $_ -and (Test-ThisCam $_) }).Count) { $env:Path = "$env:Path;$Scripts" }

# ---- 4. Claude Code
Step "检查 Claude Code"
$claude = Get-Command claude -ErrorAction SilentlyContinue | Select-Object -First 1
$localClaude = Join-Path $env:USERPROFILE ".local\bin\claude.exe"
if ($claude) {
  Ok "已经装好了：$($claude.Source)"
} elseif (Test-Path -LiteralPath $localClaude) {
  Ok "已经装好了：$localClaude"
} else {
  Note "没找到 claude 命令。cam 切换的是 Claude Code 的登录，要先装 Claude Code"
  if (Ask "现在用 Anthropic 官方安装器装 Claude Code 吗？" $false) {
    & $PyExe @PyArgs $CamPy env install -y
    if ($LASTEXITCODE -ne 0) { Warn "没装成，见上面的输出；以后可以运行 cam env install 再试" }
  } else {
    Note "以后需要时运行 cam env install"
  }
}

# ---- 5. 托盘小工具：任务栏右下角显示当前账号和额度，顺带做后台任务
Step "托盘小工具"
$ws = New-Object -ComObject WScript.Shell
$TrayOn = $false
if ((Test-Path -LiteralPath $TrayLink) -and $ws.CreateShortcut($TrayLink).Arguments -eq $TrayArgs) {
  Ok "已经装好了：$TrayLink"
  if (-not (Get-TrayProcess).Count) { Start-Tray; Ok "启动了托盘小工具" }
  $TrayOn = $true
} else {
  Note "任务栏右下角的图标上写着当前账号 5 小时额度用了百分之几，底色绿 / 黄 / 红；点开能看到所有账号，点一下切换"
  Note "登录 Windows 后自动运行，顺带在后台自动保存新登录、执行时间规则、刷新用量、额度快用完时提醒"
  if (Ask "装托盘小工具吗？" $true) {
    $lnk = $ws.CreateShortcut($TrayLink)
    $lnk.TargetPath = $Conhost
    $lnk.Arguments = $TrayArgs
    $lnk.WorkingDirectory = $Root
    $lnk.WindowStyle = 7
    $lnk.Description = "cam 托盘小工具：Claude 账号和额度"
    $lnk.Save()
    Stop-TrayProcess  # 以前从别的文件夹装过的话，换成这一份
    Start-Tray
    Ok "已装好并启动，登录 Windows 后自动运行：$TrayLink"
    Note "图标在任务栏右下角；看不到的话点 ^ 展开，可以把它拖到任务栏上常驻"
    $TrayOn = $true
  } else {
    Note "以后想装：重新运行 安装.cmd。不装的话，想在后台自动保存新登录、执行时间规则、刷新用量，可以装桌面版，或者需要时运行 cam serve / cam panel"
  }
}

# ---- 6. 环境检查
Step "环境检查（cam doctor，会联网测一下 Claude 服务，要几秒钟）"
$out = @(& $PyExe @PyArgs $CamPy doctor 2>&1 | ForEach-Object { "$_" })
if ($out.Count) { Write-Host "  $($out[0])" }
$problems = @($out | Where-Object { $_ -match '^(✗|!) ' })
if ($problems.Count) {
  $problems | ForEach-Object { Write-Host "  $_" }
  Note "上面几项要留意，运行 cam doctor 看完整结果和建议"
} else {
  Ok "没发现问题（完整结果：cam doctor）"
}

Write-Host ""
Write-Host "✓ 安装完成" -ForegroundColor Green
Write-Host ""
Write-Host "下一步（先新开一个终端，cam 命令才找得到）："
Write-Host "  · cam login 工作号    登录一个 Claude 账号，浏览器里登录完自动保存；每个账号换个名字各登录一次"
Write-Host "  · cam                 列出所有账号和额度；cam switch 名字 切换"
Write-Host "  · cam panel           在浏览器打开管理面板"
if ($TrayOn) { Write-Host "  · 任务栏右下角的托盘图标：所有账号的额度，点一下切换" }
Write-Host "  · 更新：cam update（只在你运行时更新）；卸载：安装.cmd -Uninstall"
