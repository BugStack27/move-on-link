<p align="center">
  <img src="public/assets/app-icon.png" width="96" alt="MoveOnLink" />
</p>

<h1 align="center">MoveOnLink</h1>

<p align="center">把大文件夹搬到别的盘，原位置留一个链接——C 盘瘦身，路径不变。</p>

<p align="center">
  <code>Tauri 2</code> · <code>Rust</code> · <code>Vite 7</code> · Windows 10 / 11
</p>

<p align="center">
  <a href="https://github.com/BugStack27/move-on-link/actions/workflows/release.yml"><img src="https://github.com/BugStack27/move-on-link/actions/workflows/release.yml/badge.svg" alt="Release" /></a>
</p>

## 它解决什么问题

C 盘里总有几个吃空间的大户：微信/QQ 的数据目录、Steam 库、模型缓存、视频工程。直接搬走，软件会找不到路径；不搬，C 盘见底。

MoveOnLink 干的就是这两步：把整个文件夹搬到目标盘，然后在原位置建一个 **Junction（目录联接）** 指过去。所有程序和快捷方式照旧用原来的路径，实际文件已经落在别的盘。

## 功能

- **向导式四步** — 选择源文件夹 → 选择目标盘并命名 → 预检 → 迁移，不熟命令行也能用。
- **迁移前预检** — 统计大小与文件数、检查目标盘剩余空间、探测被占用的文件、检查当前进程权限；有阻断项时不让开始。
- **两种链接** — `Junction /J`（默认，不需要开发者模式与管理权限）和 `Symlink /D`（需要管理员权限），按文件夹情况选。
- **实时进度** — 百分比、已搬字节、速度、当前文件、阶段说明；中途可以取消，已搬部分会按策略清理。
- **撤销迁移** — 把文件从目标盘搬回原位并删除链接，误操作可回退；目标文件被改动过时会给出提示。
- **迁移记录** — 本地留存每次迁移的源、目标、大小、链接类型与状态（完成 / 完成但有警告 / 已撤销），随时打开目标目录或撤销。
- **提权提示** — 启动时检测当前进程是否为管理员，未提权时提示需要提权的场景。
- **无边框窗口** — 自绘标题栏与控制按钮，跟随系统深浅色。

## 工作原理

```
迁移前： C:\Users\me\WeChat Files\        （真实文件）
迁移后： C:\Users\me\WeChat Files\  →  D:\MoveOn\WeChat Files\   （Junction 指向）
```

建链接用的是 Windows 原生能力（`junction` crate + `mklink`），不装驱动、不挂虚拟盘，资源管理器里显示的仍是普通文件夹。

## 技术栈

| 层 | 技术 |
|---|---|
| 桌面壳 | Tauri 2（WebView2） |
| 后端 | Rust（文件扫描、迁移、链接、校验、记录） |
| 前端 | 原生 JS + CSS，Vite 7 构建 |
| 数据 | 本地 JSON（迁移记录，存于应用数据目录） |

## 快速开始

环境：Windows 10 / 11、Node.js ≥ 20.19（推荐 22）、Rust ≥ 1.77、WebView2（Win11 自带）。

```bash
npm install          # 前端依赖
npm run dev          # 只起 Vite（http://localhost:1420），调界面用
npm run tauri dev    # 起桌面窗口（开发版）
```

```bash
npm run build        # 前端产物 → dist/
npm run tauri build  # 打包安装包 → src-tauri/target/release/bundle/
```

## 发布

仓库：<https://github.com/BugStack27/move-on-link>

推 `v` 前缀的 tag 会触发 GitHub Actions（[release.yml](.github/workflows/release.yml)）：校验 tag 与 `package.json` / `tauri.conf.json` 的版本一致，在 `windows-latest` 上构建 MSI 与 NSIS 安装包，挂到同名 Release 的 Assets。

```bash
git tag -a v0.0.1 -m "release: 0.0.1"   # 必须与版本号一致
git push origin v0.0.1
```

## 目录

```
index.html          应用界面（四步向导、记录页、对话框）
src/main.js         前端逻辑，通过 invoke 调 Rust 命令
src/styles.css      样式
src-tauri/          Rust 后端
  src/lib.rs        Tauri 命令：预检、迁移、取消、记录、撤销、提权检测
  src/engine.rs     迁移引擎（复制、校验、清理、建链）
  src/scan.rs       目录扫描与被占用文件探测
  src/pathutil.rs   路径工具、权限检测、链接创建
  src/records.rs    迁移记录读写
  src/model.rs      数据结构
  tauri.conf.json   窗口、CSP、打包目标与图标
.github/workflows/  推 tag 自动构建安装包
```

## 注意

- 只支持 Windows：Junction / Symlink 与路径处理依赖 Windows API。
- 迁移目标建议放在本地盘；网络盘、移动盘断开后链接会失效。
- 迁移期间请关闭正在读写该文件夹的程序，预检会把探测到的占用文件列出来。
