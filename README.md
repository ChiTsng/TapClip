# TapClip

用 iPhone 碰一下桌角的 NFC 标签，就能和 Windows 电脑交换一段剪贴板内容。

- **电脑端完全无感：** 一个 512 KB 的后台程序，空闲时 CPU 占用为 0，内存不到 2 MB。
- **iPhone 端：** 一个快捷指令，加上一个 NFC 自动化。
- **不污染电脑剪贴板：** iPhone 发来的内容在真正粘贴之前，不进 Win+V 历史；没用上就自动消失，恢复原来的剪贴板。

具体行为见 [docs/SPEC.md](docs/SPEC.md)，iPhone 端怎么配置见 [docs/iphone-setup.md](docs/iphone-setup.md)。

## 工作方式

```
iPhone 快捷指令 ──HTTP (局域网, 密钥)──► tapclip.exe ──► Windows 剪贴板
  碰 NFC 标签时触发                        共享项状态机
  读一次剪贴板                             延迟提供 / 快照恢复
```

## 目录结构

```
src/
  main.rs         启动流程
  engine.rs       共享项状态机（纯逻辑，单元测试覆盖全部规则）
  server.rs       局域网 HTTP 接口（/ping, /tap），鉴权与防暴力尝试
  bridge.rs       HTTP 线程 → UI 线程的请求转交
  config.rs       配置文件与配对密钥
  pairing.rs      配对信息文本
  logging.rs      日志（只记长度和决策，不记内容）
  win/
    app.rs        隐藏窗口、消息循环、托盘菜单
    clipboard.rs  剪贴板读写、延迟提供、快照与恢复
    tray.rs       托盘图标
    icon.rs       运行时绘制的图标
    autostart.rs  开机自启（HKCU Run）
    instance.rs   单实例
    ui.rs         Win32 小工具
scripts/
  install.ps1     编译并安装到 %LOCALAPPDATA%\Programs\TapClip
  e2e.ps1         端到端测试（模拟 iPhone，驱动真实剪贴板）
docs/
  SPEC.md         功能规格
  iphone-setup.md iPhone 端配置步骤
  manual-test.md  需要真机确认的测试清单
```

## 开发

需要 Rust（MSVC 工具链）和 Visual Studio Build Tools 的 C++ 组件。

```bash
cargo test
```

```bash
cargo clippy --all-targets
```

```bash
pwsh scripts/e2e.ps1
```

`e2e.ps1` 会实际改写剪贴板，测试期间不要复制东西。测完会还原你原来的剪贴板文字；测试用的文字会留在 Win+V 历史里。

## 安装

```bash
pwsh scripts/install.ps1 -Autostart
```

- 安装后会自动启动。第一次启动会弹出配对信息，并且 Windows 防火墙会询问是否允许，要勾选「专用网络」。
- 升级时重新运行一次安装脚本就行。
- 卸载：`pwsh scripts/install.ps1 -Uninstall`

## 文件位置

| 用途 | 位置 |
|---|---|
| 程序 | `%LOCALAPPDATA%\Programs\TapClip\tapclip.exe` |
| 配置（端口、密钥、有效时间） | `%APPDATA%\TapClip\config.toml` |
| 日志 | `%LOCALAPPDATA%\TapClip\tapclip.log` |

## 安全

- **只在局域网里工作：** 所有请求都要带密钥。密钥有 80 位随机数，比较时用常数时间。
- **防暴力猜测：** 同一 IP 连续输错 10 次，会被锁定 10 分钟。
- **限制请求大小：** 请求头最多 16 KB，文字最多 1 MB，读写都有超时。
- **不传敏感内容：** 密码管理器等程序标记为不要记录或同步的内容，不会发给手机。
