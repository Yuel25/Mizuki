# Mizuki 项目交接文档

> 最后更新：2026-08-28  
> 当前版本：0.1.0  
> 目标平台：Windows x64  
> 项目目录：`D:\projects\Mizuki`

## 1. 项目定位

Mizuki 是一款参考 Kazumi 与 Animeko 交互方式设计的 Windows 动漫追踪、收藏管理和下载应用。

当前产品方向：

- 使用 Bangumi 提供番剧周表、条目资料、评分、简介和短评。
- 默认使用本地 SQLite 管理“想看 / 在看 / 看过 / 搁置 / 抛弃”和观看进度。
- 可选使用 Bangumi Personal Access Token 导入并同步个人收藏，不采用 OAuth 回调服务。
- 使用 Mikan RSS 作为资源订阅入口。
- 使用内置 librqbit 下载磁力链接或种子资源。
- 使用无边框窗口、自定义窗口控制按钮和系统托盘。

## 2. 技术栈

### 桌面与后端

- Tauri 2
- Rust 2024 Edition
- reqwest：Bangumi 与 RSS 网络请求
- rusqlite：本地 SQLite 数据库
- keyring：Windows 凭据管理器中的 Bangumi Access Token
- librqbit：内置 BT 下载
- rss：RSS 解析
- Tauri tray、single-instance、opener 插件

### 前端

- React 19
- TypeScript 5.8
- Vite 7
- 原生 CSS，无 UI 组件库

## 3. 目录说明

```text
Mizuki/
├─ assets/icon/              应用图标主图与多尺寸 PNG、ICO
├─ public/                   前端公开资源
├─ src/
│  ├─ App.tsx               当前主要界面与前端业务逻辑
│  ├─ App.css               主界面样式
│  ├─ Frameless.css         无边框窗口、侧边栏、滚动条等样式
│  ├─ Detail.css            详情页、短评、返回按钮样式
│  ├─ Manager.css           RSS 与下载管理界面样式
│  └─ DownloadExtras.css    手动添加下载等补充样式
├─ src-tauri/
│  ├─ capabilities/         Tauri 前端能力与本地路径访问权限
│  ├─ src/lib.rs            Tauri 初始化、Commands、托盘、下载会话
│  ├─ src/bangumi.rs        Bangumi API 封装和响应转换
│  ├─ src/db.rs             SQLite 表结构与数据访问
│  ├─ src/feeds.rs          RSS 获取与解析
│  ├─ src/matcher.rs        RSS 标题匹配规则
│  ├─ src/models.rs         Rust 数据模型
│  ├─ icons/                Tauri 打包图标
│  └─ tauri.conf.json       窗口与 NSIS 打包配置
├─ oauth-broker/            早期 OAuth 代理实验，当前桌面流程未使用
├─ README.md                项目使用和构建说明
└─ HANDOFF.md               本交接文档
```

注意：截至本文档更新时，`D:\projects\Mizuki` 不是 Git 仓库，执行 `git status` 会失败。继续开发前建议初始化 Git，并先提交当前可构建版本作为基线。

## 4. 当前已完成能力

### 4.1 应用图标与安装包

- 已完成 Mizuki 月牙、播放轨道与下载箭头组合图标。
- 已生成 16、24、32、48、64、128、256、512、1024 PNG。
- 已生成 Windows `icon.ico`。
- Tauri 和系统托盘使用应用图标。
- 已配置 NSIS x64 安装包。

当前安装包：

```text
D:\projects\Mizuki\src-tauri\target\release\bundle\nsis\Mizuki_0.1.0_x64-setup.exe
```

### 4.2 无边框窗口

- `decorations: false`，取消系统标题栏。
- 顶部存在透明拖拽区域。
- 右上角提供最小化、最大化/还原、关闭按钮。
- 关闭窗口默认隐藏到托盘。
- 托盘菜单包含显示、暂停全部下载、退出。
- 单实例启动时会显示并聚焦已有窗口。
- 详情面板不再在右上角显示额外的关闭图标。
- 详情面板左上角使用“← 返回”按钮，滚动时保持可见。

### 4.3 Bangumi 番剧数据

- 周表：`GET https://api.bgm.tv/calendar`
- 条目详情：`GET https://api.bgm.tv/v0/subjects/{subject_id}`
- 用户信息：`GET https://api.bgm.tv/v0/me`
- 用户收藏：`GET https://api.bgm.tv/v0/users/{username}/collections`
- 写入收藏：`POST https://api.bgm.tv/v0/users/-/collections/{subject_id}`
- 短评：`GET https://next.bgm.tv/p1/subjects/{subject_id}/comments`

条目详情打开时会补充：

- 完整简介
- `total_episodes` / `eps` 集数
- 评分与排名
- 高清封面
- 短评用户头像、昵称、评分和正文

周表接口经常缺失总集数。卡片遇到 `episodes == 0` 时会调用 `get_subject_detail` 补查集数。

### 4.4 Bangumi Access Token 同步

- 设置页可打开 `https://next.bgm.tv/demo/access-token`。
- 用户粘贴 Personal Access Token 后，先调用 `/v0/me` 验证。
- Token 只保存在 Windows 凭据管理器：

```text
service: app.mizuki.desktop
account: bangumi-access-token
```

- Token 不写入 SQLite、前端状态或普通配置文件。
- 可分页导入动画收藏，每页 50 条。
- 导入内容包括收藏状态、`ep_status` 和条目资料。
- 收藏条目资料写入 `cached_subjects`，因此不在当前周表中的旧番也可以出现在“我的追番”。
- 修改收藏状态时先写本地，然后后台写回 Bangumi。
- 断开 Bangumi 只删除凭据，不删除本地收藏。

### 4.5 本地数据库

数据库位于 Tauri 应用数据目录的 `mizuki.sqlite3`，由 `Database::open` 自动创建和迁移基础表。

当前表：

- `rss_feeds`
- `rss_items`
- `downloads`
- `local_collections`
- `cached_subjects`
- `settings`

其中：

- `local_collections` 保存条目 ID、收藏状态、观看集数和更新时间。
- `cached_subjects` 保存 Bangumi 收藏返回的条目 JSON，支持追番页展示非周表条目。

### 4.6 Mikan RSS 与下载

- 可以填写 RSS URL 并验证、保存订阅源，应用启动时自动恢复列表和历史条目。
- 支持单个/全部刷新、启用、停用、自动下载开关和删除订阅。
- 首次添加订阅只建立历史基线，不会补下旧资源；后续刷新只处理新增 GUID。
- 前端每 15 分钟自动刷新启用的订阅源。
- 新资源可手动下载；启用自动下载后会自动进入 librqbit 队列。
- 支持查看最近 RSS 资源、来源页面、发布时间及是否已经入队。
- 支持 magnet、HTTP、HTTPS 种子来源，也可在下载页手动添加任务。
- 下载页每 2 秒刷新进度、上下行速度和状态。
- 支持暂停、继续、打开目录、移除任务，以及删除任务和文件。
- “目录”按钮可打开系统下载目录下的 `Mizuki`；对应的 Tauri opener 权限仅放行 `$DOWNLOAD/Mizuki/**`。
- 完成任务显示“▶ 播放”按钮，通过种子元数据查找该任务内体积最大的视频文件，并交给 Windows 默认播放器打开。
- 可播放扩展名目前包括 `mkv`、`mp4`、`webm`、`avi`、`mov`、`m4v`、`ts`；找不到文件、文件被移动或系统打开失败时会显示错误提示。
- 下载完成后自动暂停，停止做种。
- 应用重启时会尝试恢复等待中、下载中和已暂停任务。
- 托盘“暂停全部下载”已接入实际下载会话。
- 下载目录为系统下载目录下的 `Mizuki`。

### 4.7 当前界面

侧边栏页面：

- 今日：按周一至周日查看 Bangumi 周表；当天处于“在看/想看”的收藏会单独显示在顶部“我的追番”区，其余条目显示在“今日全部”。
- 追番：按想看、在看、看过、搁置、抛弃筛选。
- RSS：添加 Mikan RSS。
- 下载：展示任务、速度、进度和完成状态。
- 设置：Bangumi Token、下载和后台选项。

## 5. 关键实现约定

### 5.1 收藏状态映射

| 前端状态 | Bangumi type | 中文 |
|---|---:|---|
| `wish` | 1 | 想看 |
| `collect` | 2 | 看过 |
| `doing` | 3 | 在看 |
| `on_hold` | 4 | 搁置 |
| `dropped` | 5 | 抛弃 |

### 5.2 星期映射

前端使用：

```text
0 周一 ... 6 周日
```

Bangumi 周表返回的 weekday ID 会在 `bangumi::calendar` 中转换为上述格式。缓存收藏条目如果没有周表信息，`air_weekday` 为 `-1`，仅用于追番页，不会进入任意星期分类。

### 5.3 前后端字段

Rust 模型使用 `#[serde(rename_all = "camelCase")]`，前端主要字段包括：

```text
nameCn, airWeekday, updateState, downSpeed, outputPath, lastCheckedAt
```

Bangumi 原始详情与短评作为 JSON 直接返回时仍使用 API 原字段，例如：

```text
total_episodes, rating.score, rating.rank
```

## 6. 已解决问题记录

- 生成并接入多尺寸应用与托盘图标。
- 修复无边框窗口无法拖拽。
- 增加右上角最小化、最大化、关闭按钮。
- 修复托盘图标缺失。
- 将不协调的白色滚动条改为紫色主题滚动条。
- 放弃需要应用密钥和回调端口的 Bangumi OAuth 方案。
- 改用 Kazumi 风格的 Personal Access Token。
- 修复 Bangumi 收藏接口错误按数组解析的问题；实际返回分页对象 `{ data, total, limit, offset }`。
- 修复周表简介为空：详情打开时调用 `/v0/subjects/{id}`。
- 修复总集数缺失：兼容 `total_episodes`、`eps`、`eps_count`，并对卡片按需补查。
- 修复短评接口 404：原 `/v0/subjects/{id}/comments` 不存在，改为当前可用的 `/p1/subjects/{id}/comments`。
- 修复详情关闭按钮和窗口按钮在右上角堆叠。
- 将“今日”页中的在看/想看番剧提取为独立置顶区块，避免在完整周表中查找收藏。
- 修复下载任务“目录”按钮无响应：补充受下载目录范围限制的 `opener:allow-open-path` 权限，并在前端捕获错误。
- 为完成任务增加“播放”按钮；新增 `download_playback_path` Command，使用对应 librqbit 任务的元数据定位主视频，避免误开字幕或 sample 文件。

## 7. 已知问题与未完成项

以下能力尚未达到可发布状态：

### 高优先级

1. Bangumi 写回错误被后台任务忽略
   - `set_collection` 会先成功写本地，再 spawn 写回 Bangumi。
   - 写回失败目前没有重试队列、状态标记或 UI 提示。
   - 建议恢复可靠的 `sync_queue`，实现指数退避、失败提示和手动重试。

2. 观看进度不可编辑
   - UI 只显示 `watched / episodes`。
   - 尚无“看完一集”、集数选择器或 Bangumi episode collection 写回。

3. 集数按卡片逐个请求
   - 一个星期可见条目较多时会产生多次详情请求。
   - 建议后端增加详情缓存、并发限制和失效时间，或改用批量季度数据源。

4. 短评使用 Bangumi Private API
   - `/p1/` 接口当前真实可用，但属于私有 API，兼容性不如 `/v0/`。
   - 请求失败时应提供跳转 Bangumi 网页的降级方案。

### 中优先级

5. RSS 高级匹配规则尚未接入设置界面
   - 基础订阅、增量刷新和自动下载已经闭环。
   - `matcher.rs` 已有包含、排除、分辨率和字幕组匹配器，但尚未为每个订阅保存规则，也没有规则编辑界面。

6. 下载器仍缺少高级 BT 设置
   - 基础添加、恢复、暂停、继续、播放、删除和目录打开已经闭环。
   - 尚未提供限速、连接数、端口、代理、文件选择和下载优先级。

7. 播放依赖活动下载任务的 librqbit 元数据
   - `download_playback_path` 从内存中的 `ManagedTorrent` 获取文件清单。
   - 如果历史完成任务未成功恢复到下载会话，会提示“任务不存在或尚未恢复”。
   - 多视频合集目前默认打开体积最大的文件，尚无剧集/文件选择界面。

8. 设置项多为静态展示
   - 下载目录选择、并发数、停止做种、刷新间隔、开机启动均未保存或生效。
   - UI 当前正确展示“系统下载目录\\Mizuki”，但尚不支持用户修改目录。

9. 前端仍包含演示数据
   - Bangumi 或 Tauri Command 请求失败时会保留 `demoSubjects`、`demoDownloads`。
   - 发布前应改为空状态或明确标记演示模式，避免用户将假数据误认为真实数据。

### 工程化

10. 当前目录不是 Git 仓库。
11. 没有端到端测试和前端组件测试。
12. Rust 文件中部分代码为高度压缩的单行形式，建议格式化和拆分。
13. `oauth-broker/` 已废弃但仍保留，应决定归档或删除。
14. `tauri-plugin-deep-link` 已配置 `mizuki://`，但当前业务未使用。
15. CSP 当前为 `null`，发布前应配置网络和图片来源白名单。

## 8. 建议后续开发顺序

1. 初始化 Git，提交当前可构建基线。
2. 增加后端 API/数据库集成测试，重点覆盖收藏分页响应和缓存合并。
3. 完成观看进度编辑及 Bangumi 章节同步。
4. 建立可靠同步队列与前端同步状态提示。
5. 为 RSS 增加每订阅的字幕组、分辨率、语言和排除规则界面。
6. 为下载器增加限速、代理、端口、文件选择和优先级设置。
7. 让设置页所有选项真实落库并生效。
8. 清除演示数据，完善加载骨架、错误提示和离线状态。
9. 配置 CSP、日志脱敏、崩溃恢复和正式版本号。
10. 补充签名、升级与发布流程。

## 9. 开发与构建

环境要求：

- Node.js 20+
- Rust 1.85+
- Windows WebView2
- NSIS（Tauri 构建流程当前环境已可用）

安装依赖并启动：

```powershell
cd D:\projects\Mizuki
npm install
npm run tauri dev
```

仅构建前端：

```powershell
npm run build
```

检查 Rust：

```powershell
cd D:\projects\Mizuki\src-tauri
cargo check --locked
```

生成 Windows 安装包：

```powershell
cd D:\projects\Mizuki
npm run tauri build
```

最近一次验证结果：

- `npm run build`：通过（2026-08-28）
- `cargo check`：通过（2026-08-28）
- `npm run tauri build`：通过（2026-08-28）
- Release 可执行文件：`D:\projects\Mizuki\src-tauri\target\release\mizuki.exe`
- NSIS 安装包：`D:\projects\Mizuki\src-tauri\target\release\bundle\nsis\Mizuki_0.1.0_x64-setup.exe`

## 10. 外部接口与参考资料

- Bangumi API：https://github.com/bangumi/api
- Bangumi OpenAPI：https://github.com/bangumi/api/blob/master/open-api/v0.yaml
- Bangumi Private API：https://bangumi.github.io/dev-docs/
- Bangumi Token 生成：https://next.bgm.tv/demo/access-token
- Mikan Project：https://mikanani.me/
- Kazumi：https://github.com/Predidit/Kazumi
- Animeko：https://github.com/open-ani/animeko

## 11. 交接检查清单

- [ ] 安装当前 NSIS 包并确认能启动。
- [ ] 检查无边框窗口拖拽、最小化、最大化和关闭到托盘。
- [ ] 在设置页验证 Bangumi Access Token。
- [ ] 点击“立即同步收藏”，确认各分类数量和观看集数。
- [ ] 打开周表条目，确认简介、总集数和短评。
- [ ] 添加一个 Mikan RSS，确认数据库写入。
- [ ] 添加测试磁力任务，确认速度和进度刷新。
- [ ] 下载完成后点击“目录”，确认资源管理器打开 `下载\Mizuki`。
- [ ] 下载完成后点击“▶ 播放”，确认 Windows 默认播放器打开正确视频。
- [ ] 使用包含多个视频的合集种子验证“最大文件”默认策略是否符合预期。
- [ ] 初始化 Git 并提交基线。
