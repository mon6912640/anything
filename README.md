# anything

无需管理员权限的本地文件名极速搜索（类似 Everything），单个 exe、无任何依赖，适合内网/离线机器。

## 文件

| 文件 | 说明 |
|------|------|
| `anything.exe` | 主程序（图形界面 + 托盘 + 后台索引） |
| `ae.exe` | 命令行搜索工具（类似 Everything 的 `es.exe`），供脚本和第三方程序调用 |

两个文件放在同一目录即可，直接双击 `anything.exe` 运行。不需要安装，不需要管理员权限，不需要 .NET 或 VC++ 运行库。

**便携模式**：exe 同目录下有 `settings.ini` 时（发布的便携版压缩包自带一份），所有数据都保存在这个文件夹里，可以整个文件夹拷走或放 U 盘。没有 `settings.ini` 时数据保存在 `%LOCALAPPDATA%\anything\`。

数据文件：

- `settings.ini`：设置（索引哪些盘、排除目录、快捷键等），首次运行自动生成，里面有注释
- `index.bin`：索引文件
- `ui.ini`：界面状态（预览面板开关、宽度），程序自动维护

## 工作原理

- **首次运行**：多线程遍历所有本地磁盘建立索引。固态盘约每秒 20～40 万个文件，机械盘慢得多；每个盘扫完就可以搜，不用等全部扫完
- **之后启动**：不到 1 秒加载上次的索引，马上可以搜；后台再低优先级重扫一遍，校正程序关闭期间的变化（可在设置里关闭）
- **运行期间**：实时监视文件变化（新建、删除、改名、移动、修改），索引随时最新
- 没有管理员权限读不了 NTFS 主文件表（MFT），所以首次建索引比 Everything 慢，但搜索速度相当

## 搜索语法

| 写法 | 含义 |
|------|------|
| `abc def` | 同时包含 abc 和 def（空格 = 并且） |
| `a\|b` | 包含 a 或 b |
| `!abc` | 不包含 abc |
| `*.psd`　`a?c` | 通配符，匹配完整文件名 |
| `xmwj` | 拼音首字母，可搜到“项目文件” |
| `work\`　`\src\ *.rs` | 含 `\` 的词在完整路径中匹配 |
| `path:abc` | 在完整路径中匹配 |
| `regex:^a.*\.rs$` | 正则表达式（`regex:path:...` 匹配完整路径） |
| `ext:png;jpg` | 按扩展名 |
| `file:`　`folder:` | 只要文件 / 只要文件夹 |
| `size:>10mb`　`size:1mb..1gb` | 按大小 |
| `dm:today`　`dm:week`　`dm:2026-10-01`　`dm:>2026-01-01`　`dm:2026-01..2026-06` | 按修改时间（today yesterday week month year） |
| `"a b"` | 引号包住含空格的词 |

## 快捷键

| 按键 | 作用 |
|------|------|
| `Ctrl+Alt+F`（可改） | 全局呼出 / 隐藏 |
| `↓` | 从搜索框进入结果列表 |
| `Enter` / `Ctrl+Enter` | 打开 / 打开所在文件夹并选中 |
| `Ctrl+C` / `Ctrl+Shift+C` | 复制完整路径 / 复制文件（可在资源管理器粘贴） |
| `Esc` | 清空搜索；再按一次隐藏窗口 |
| `F3` | 显示 / 隐藏右侧图片预览面板 |
| `F5` | 立即重建索引 |
| `F1` | 帮助 |

点列标题可按名称、路径、大小、修改时间排序；结果可以直接拖到资源管理器或其他程序。关闭窗口只是隐藏到托盘，退出请用托盘菜单或“文件 → 退出”。

## 图片预览

右侧预览面板（`F3` 或“查看 → 图片预览面板”开关，拖动左边的分隔条调宽度）：选中图片立即显示，用 ↑↓ 在同名图片之间切换就能逐个对比。

- 小图按原尺寸 1:1 显示（不放大糊掉），大图按面板大小高质量缩小，下面标出缩放比例
- 透明区域显示灰白棋盘格，图片边界有细框，方便看清透明边缘
- 显示尺寸、格式、位深、是否含透明、文件大小、修改时间、所在目录
- 双击预览图打开文件，右键与列表右键菜单相同；文件被修改后自动刷新

支持的格式（全部用系统自带组件或内置解码，不需要安装任何东西）：

| 格式 | 说明 |
|------|------|
| PNG、JPG、BMP、GIF、TIFF、ICO | Windows 图像组件（GIF 显示第一帧，ICO 显示最大尺寸） |
| WebP、HEIC、AVIF | Windows 11 自带或安装了对应“图像扩展”时可用 |
| DDS | 支持 DXT1~5（BC1~3）、BC4、BC5、未压缩格式；BC6H/BC7 暂不支持 |
| TGA | 内置解码，支持 RLE 压缩、调色板、灰度、透明通道 |
| PSD、PSB | 内置解码文件中的合并图像，不需要 Photoshop（保存时需勾选“最大兼容”，这是默认设置）；支持 RGB/CMYK/灰度/索引色，8/16/32 位 |
| SVG | Direct2D 渲染（支持 SVG 常用子集，不支持文字和滤镜），按面板大小渲染 |
| 其他 | 尝试借用资源管理器的缩略图（装了对应缩略图扩展时可用） |

解码在后台线程进行，快速翻动时只解最后一张；最近看过的图会缓存，来回切换时立即显示。

## 命令行 ae.exe（供第三方程序调用）

```
ae [选项] 搜索词...
```

| 选项 | 说明 |
|------|------|
| `-n <N>` / `-o <N>` | 最多输出 N 条 / 跳过前 N 条 |
| `-s name\|path\|size\|date\|none`、`-desc` | 排序列、倒序 |
| `-path <目录>` | 只搜该目录下 |
| `-regex <正则>` | 正则匹配文件名 |
| `-count` | 只输出匹配数量 |
| `-size`、`-dm` | 附加输出大小（字节）、修改时间 |
| `-name` | 只输出文件名 |
| `-csv`、`-tsv`、`-json` | 输出格式（默认每行一个完整路径） |
| `-0` | 每条以 `\0` 结尾 |
| `-out <文件>` | 写入文件（UTF-8） |
| `-ansi` | 以系统代码页（GBK）输出，兼容老程序 |
| `-offline` | 不连接 anything，直接读索引文件 |
| `-status`、`-rescan` | 查看索引状态、让 anything 重建索引 |

例子：

```bat
ae -n 20 -s date -desc ext:log dm:today
ae -json -size *.psd > psd.json
ae -count file: size:>1gb
ae -path D:\Work -csv -size -dm ext:xlsx;docx -out list.csv
```

- anything 运行时，ae 通过命名管道 `\\.\pipe\anything-<用户名>` 查询，毫秒级、结果实时
- anything 未运行时，ae 自动读取上次保存的索引（stderr 会有提示）
- 输出到控制台时自动正确显示中文；重定向到文件或管道时默认是 UTF-8，老程序需要 GBK 时加 `-ansi`
- 退出码：0 成功，1 参数错误，2 无法获取索引

### 其他语言直接调用管道

不想启动 ae.exe 的话，也可以直接连接命名管道：发送若干行 `键\t值`，以空行结束；返回第一行 `OK\t总数\t耗时毫秒`，之后每行 `d或f\t大小\t修改时间(FILETIME)\t完整路径`。

```python
# Python 示例
import os
name = r"\\.\pipe\anything-" + os.environ["USERNAME"]
with open(name, "r+b", buffering=0) as p:
    p.write("q\t*.psd\nsort\tdate\ndesc\t1\nmax\t10\n\n".encode("utf-8"))
    for line in p.read().decode("utf-8").splitlines()[1:]:
        kind, size, mtime, path = line.split("\t", 3)
        print(path)
```

可用的键：`cmd`（search / status / rescan）、`q`、`sort`（name / path / size / date / none）、`desc`（0/1）、`offset`、`max`。

## 设置（settings.ini）

```ini
roots=                 ; 要索引的目录，| 分隔；留空 = 所有本地固定磁盘
excludes=C:\Windows\WinSxS|$Recycle.Bin|System Volume Information
hotkey=Ctrl+Alt+F
rescan_on_start=1      ; 启动时后台重扫
threads=12             ; 固态盘扫描线程
hdd_threads=4          ; 机械盘扫描线程
pinyin=1               ; 拼音首字母搜索
```

修改后在菜单“文件 → 重新加载设置”或重启生效。“文件 → 开机自动启动”写入当前用户的注册表启动项，不需要管理员权限。

## 编译

需要 Rust（stable，`x86_64-pc-windows-msvc` 目标）和 VS Build Tools（含 Windows SDK）。

```
cargo build --release
```

产物在 `target\x86_64-pc-windows-msvc\release\`，静态链接 C 运行库，可直接拷到其他 Win10/11 机器运行。
