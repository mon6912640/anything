# 编译并打包成便携版 dist\anything-<版本>-portable.zip，拷到内网机解压即可使用
# 包里带一份默认 settings.ini：exe 同目录有 settings.ini 时进入便携模式，索引和设置都存在这个文件夹里
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

# 让 ae.exe 在包目录里生成默认 settings.ini（离线模式读不到索引会返回 2，忽略）
$env:ANYTHING_DATA_DIR = $out
$ErrorActionPreference = 'Continue'
& "$out\ae.exe" -offline -count x 2>&1 | Out-Null
$ErrorActionPreference = 'Stop'
Remove-Item Env:ANYTHING_DATA_DIR
if (-not (Test-Path "$out\settings.ini")) { throw "生成 settings.ini 失败" }

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$out\*" -DestinationPath $zip
Get-ChildItem $out, $zip | Format-Table Name, Length
Write-Host "已生成 $zip"
