# 编译并打包到 dist\anything-<版本>.zip，拷到内网机解压即可使用
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "编译失败" }

$ver = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$out = Join-Path $PSScriptRoot "dist\anything-$ver"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null

$rel = "target\x86_64-pc-windows-msvc\release"
Copy-Item "$rel\anything.exe", "$rel\ae.exe", README.md -Destination $out

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$out\*" -DestinationPath $zip
Get-ChildItem $out, $zip | Format-Table Name, Length
Write-Host "已生成 $zip"
