# 嘿哈嘚 自定义音频同步 — AstroBox v2 插件
该插件包括该介绍均为AI编写

向「嘿哈嘚」手表快应用（`com.huashu.heihade`）同步自定义音频的 AstroBox v2 插件。

通过 `interconnect` 接口把本地音频文件**分块（base64）**推送到手表快应用，快应用端
（`src/common/audiosync.js` + `src/pages/custom/custom.ux`）负责接收、存储与播放。

## 功能

- 列出/选择已连接设备（自动注册 `register-interconnect-recv`）
- 播放模式选择（插件决定）：单音频 / 多音频（多段音效循环 + 进度条）
- 添加多个音频文件 + 可选封面图片，同步到手表
- 分块同步（定时器节流，避免灌满 QAIC/BLE 队列）
- 手表端清单自动上报 → 插件侧显示已同步列表
- 插件可删除指定音频 / 清空全部；手表端也可长按删除，双向同步
- 手表端点击自定义音频直接进入统一播放器（手势触发 + 图片/进度条展示）

## 环境准备

```bash
# 安装 Rust（Windows 用 MSVC 工具链即可）
# https://www.rust-lang.org/learn/get-started

# 添加 wasm32-wasip2 目标
rustup target add wasm32-wasip2

# 安装 Python 3（构建脚本依赖）
# https://www.python.org/downloads/
```

## 构建

```bash
cd astrobox-plugin

# Debug 构建到 dist 文件夹
python scripts/build_dist.py

# Release 构建并打包 .abp 插件包
python scripts/build_dist.py --release --package
```

产物在 `dist/` 下：`astrobox_plugin_heihade_audiosync.wasm`（入口）与
`manifest.json`（插件描述），Release 打包得到 `.abp` 插件包。

## 安装到 AstroBox

1. 在 AstroBox（手机/PC 端）安装插件（导入 `.abp` 或插件目录）
2. 首次使用时授权插件权限（`device` / `interconnect` / `register_interconnect_recv`）
3. 手表与手机/电脑保持连接，打开插件：
   - 「刷新设备」→ 选择目标设备
   - 「播放模式」→ 单音频 / 多音频
   - 「添加音频文件」→ 选择本地音频（多音频模式可添加多个）
   - 「选择封面图片」（可选）
   - 「同步到手表」→ 等待进度完成
   - 「手表端已同步」列表 → 可删除指定音频 / 清空全部
4. 在手表端打开「嘿哈嘚」→ 菜单「自定义音频」：
   - 点击音频 → 直接进入统一播放器（手势触发播放）
   - 长按音频 → 删除（自动同步回插件列表）

> 注意：interconnect 通信要求快应用与宿主插件包名一致（均为 `com.huashu.heihade`），
> 且快应用签名需与宿主环境匹配（详见 Vela interconnect 文档）。

## 传输协议（插件 ↔ 快应用）

均为 JSON，`type` 恒为 `audiosync`：

| 方向 | 动作 | 消息要点 |
|------|------|----------|
| 插件→手表 | 开始 | `{"action":"start","id":"..","name":"..","mode":"single"\|"sequence","display":"image"\|"text","imageName":"..","duration":N,"cooldown":N,"totalSteps":N,"chunks":N,"units":[{"kind":"audio"\|"image","file":"..","duration":N},...]}` |
| 插件→手表 | 单元开始 | `{"action":"unit-start","id":"..","unitIndex":i,"kind":"audio"\|"image","file":"..","chunks":N}` |
| 插件→手表 | 数据块 | `{"action":"chunk","id":"..","unitIndex":i,"index":j,"data":"<base64>"}` |
| 插件→手表 | 结束 | `{"action":"end","id":"..","ok":true}` |
| 插件→手表 | 删除 | `{"action":"delete","id":"<soundId>"}` |
| 插件→手表 | 清空 | `{"action":"clear"}` |
| 手表→插件 | 清单上报 | `{"action":"manifest","sounds":[{id,name,mode,file,size},...]}` |

每个分块承载约 3000 字节数据（base64 约 4000 字符），每次定时器 tick（100ms）发送 4 块。

## 目录结构

```
astrobox-plugin/
├── Cargo.toml
├── manifest.json          # 插件描述（入口 wasm、权限等）
├── scripts/build_dist.py  # 构建/打包脚本（官方模板）
├── wit/                   # WIT 接口定义（官方 AstroBox-Plugin-WIT）
│   ├── main.wit
│   └── deps/
├── src/
│   ├── lib.rs             # 插件入口（lifecycle / event）
│   ├── logger.rs          # tracing 日志
│   ├── state.rs           # 全局状态（设备/文件/传输进度）
│   ├── transfer.rs        # 分块同步核心逻辑
│   └── ui.rs              # ui-v3 界面与事件处理
└── icon.png
```
