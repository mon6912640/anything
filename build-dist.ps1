# 编译并打包成便携版 dist\anything-<版本>-portable.zip，拷到内网机解压即可使用
# portable.flag 仅标记便携模式；用户 settings.ini、索引和界面状态绝不随包分发
# 注意：本文件需保存为带 BOM 的 UTF-8，否则 Windows PowerShell 5.1 会把中文读成乱码
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "编译失败" }

$ver = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$out = Join-Path $PSScriptRoot "dist\anything-$ver-portable"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null

$rel = "target\x86_64-pc-windows-msvc\release"
Copy-Item "$rel\anything.exe", "$rel\ae.exe", README.md -Destination $out
New-Item -ItemType File -Path "$out\portable.flag" | Out-Null

# 防止以后改打包脚本时误把用户数据带进压缩包
$userData = @('settings.ini', 'settings.ini.bak', 'index.bin', 'ui.ini')
foreach ($name in $userData) {
    if (Test-Path (Join-Path $out $name)) { throw "发布包不得包含用户数据：$name" }
}

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$out\*" -DestinationPath $zip
Get-ChildItem $out, $zip | Format-Table Name, Length
Write-Host "已生成 $zip"
