<#
.SYNOPSIS
    生成自动升级用的 latest.json。

.DESCRIPTION
    客户端请求的端点固定在 releases/latest/download/latest.json，
    所以这个文件必须作为「最新 Release」的附件上传（不能是 draft / prerelease）。

    下面三处弄错了都不会在打包时报错，只会在用户点「立即升级」那一刻暴露：

      1. signature 放 .sig 文件**原文**。它本身已经是 base64 —— cargo-packager 的
         sign_file_with_secret_key 里做了 STANDARD.encode(signature_box.to_string())。
         升级器拿到这个字段后还会再解一次 base64，所以这里既不能再套一层编码，
         也不能换成别种包装。
      2. signature 不能带尾部换行：base64 的 STANDARD 引擎不容忍空白字符。
         所以下面要 Trim。
      3. 输出不能带 BOM，否则 serde_json 解析会失败。

.PARAMETER Repository
    owner/repo 形式，用于拼下载地址。

.PARAMETER Tag
    形如 v0.1.1 的 tag，用于拼下载地址。

.PARAMETER DistDir
    打包产物目录，默认 dist。

.PARAMETER Notes
    展示给用户的更新说明。

.EXAMPLE
    ./scripts/gen-update-manifest.ps1 -Repository zephyraluco/rastflow -Tag v0.1.1
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Repository,
    [Parameter(Mandatory)][string]$Tag,
    [string]$DistDir = 'dist',
    [string]$Notes = '详见本 Release 的说明'
)

$ErrorActionPreference = 'Stop'

$cargoToml = Join-Path $PSScriptRoot '..\Cargo.toml'

# 版本号以 Cargo.toml 为唯一来源：安装包文件名、exe 资源里的版本号都源自它
$m = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw "在 $cargoToml 里找不到 version" }
$version = $m.Matches[0].Groups[1].Value

# tag 与版本号必须一致。对不上的后果不是「难看」，而是「装了新版仍提示有更新」，
# 同时 Cargo.toml 里的 allowDowngrades=false 又让安装器拒绝退回旧版。
$expectedTag = "v$version"
if ($Tag -ne $expectedTag) {
    throw "tag 是 $Tag，但 Cargo.toml 的 version 对应 $expectedTag —— 两者必须一致"
}

$pkg = Get-ChildItem $DistDir -Filter '*-setup.exe' | Select-Object -First 1
if (-not $pkg) { throw "$DistDir 下没有找到 NSIS 安装包（文件名应以 -setup.exe 结尾）" }

$sigPath = "$($pkg.FullName).sig"
if (-not (Test-Path $sigPath)) { throw "缺少签名文件 $sigPath —— 打包时签名没生效？" }
$sig = (Get-Content $sigPath -Raw).Trim()

$manifest = [ordered]@{
    version   = $version
    notes     = $Notes
    # 升级器按 RFC3339 解析；用固定格式避免本地时区与小数位带来的歧义
    pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    url       = "https://github.com/$Repository/releases/download/$Tag/$($pkg.Name)"
    signature = $sig
    format    = 'nsis'
}

$out = Join-Path $DistDir 'latest.json'
$manifest | ConvertTo-Json -Depth 3 | Set-Content -Path $out -Encoding utf8NoBOM

Write-Host "已生成 $out（version=$version, 安装包=$($pkg.Name), 签名 $($sig.Length) 字符）"
Get-Content $out
