# Mizuki 项目交接文档

> 最后更新：2026-08-28  
> 当前版本：0.2.0  
> 目标平台：Windows x64  
> 项目目录：`D:\projects\Mizuki`（已是 Git 仓库，main 分支）

## 1. 项目定位

Mizuki 是一款参考 Kazumi 与 Animeko 交互方式设计的 Windows 动漫追踪、收藏管理和下载应用。

当前产品方向：

- 使用 Bangumi 提供番剧周表、条目资料、评分、简介和短评。
- 默认使用本地 SQLite 管理“想看 / 在看 / 看过 / 搁置 / 抛弃”和观看进度。
- 可选使用 Bangumi Personal Access Token 导入并同步个人收藏（含观看集数写回），不采用 OAuth 回调服务。
- 使用 Mikan RSS 作为资源订阅入口，支持每订阅匹配规则与自动下载。
- 使用内置 librqbit 下载磁力链接或种子资源，支持限速、端口、连接数与并发排队。
- 使用无边框窗口、自定义窗口控制按钮和系统托盘。

## 2. 技术栈

### 桌面与后端

- Tauri 2
- Rust 2024 Edition
- reqwest：Bangumi 与 RSS 网络请求
- rusqlite：本地 SQLite 数据库
- keyring：Windows 凭据管理器中的 Bangumi Access Token
- librqbit：内置 BT 下载（会话限速、监听端口、UPnP）
- rss：RSS 解析
- Tauri tray、single-instance、opener、autostart、updater 插件

### 前端

- React 19
- TypeScript 5.8
- Vite 7
- 原生 CSS，无 UI 组件库

## 3. 目录说明

```text
Mizuki/
├─ .github/workflows/         推送 v* 标签自动构建 Release 的 CI
├─ .keys/                     Updater 签名密钥（已 gitignore，绝不能提交）
├─ assets/icon/              应用图标主图与多尺寸 PNG、ICO
├─ public/                   前端公开资源
├─ src/
│  ├─ App.tsx               全部界面与前端业务逻辑（含同步状态、规则编辑器、播放选集）
│  ├─ App.css               主界面样式
│  ├─ Frameless.css         无边框窗口、侧边栏、滚动条等样式
│  ├─ Detail.css            详情页、短评、进度编辑、返回按钮样式
│  ├─ Manager.css           RSS 与下载管理界面样式
│  ├─ DownloadExtras.css    手动添加下载等补充样式
│  ├─ RssGroups.css         RSS 分组与订阅规则编辑器样式
│  ├─ Enhancements.css      搜索框、骨架屏、同步状态条等增量样式
│  ├─ Theme.css             深色/浅色主题变量与覆盖
├─ src-tauri/
│  ├─ capabilities/         Tauri 前端能力与本地路径访问权限
│  ├─ src/lib.rs            Tauri 初始化、Commands、托盘、下载会话、同步队列 worker、设置
│  ├─ src/bangumi.rs        Bangumi API 封装和响应转换（含收藏分页/映射纯函数与测试）
│  ├─ src/db.rs             SQLite 表结构与数据访问（含内存库测试）
│  ├─ src/feeds.rs          RSS 获取与解析
│  ├─ src/matcher.rs        RSS 标题匹配规则
│  ├─ src/models.rs         Rust 数据模型（Subject、RssFeed+FeedRule、DownloadTask 等）
│  ├─ icons/                Tauri 打包图标
│  └─ tauri.conf.json       窗口、CSP、updater 与 NSIS 打包配置
├─ oauth-broker/            早期 OAuth 代理实验，当前桌面流程未使用（待归档或删除）
├─ RELEASE.md               发布流程（版本号、签名、CI、Authenticode）
├─ README.md                项目使用和构建说明
└─ HANDOFF.md               本交接文档
```

## 4. 当前已完成能力

### 4.1 应用图标与安装包

- 已完成 Mizuki 月牙、播放轨道与下载箭头组合图标（多尺寸 PNG + ICO）。
- 已配置 NSIS x64 安装包与 Updater 升级包签名（见 `RELEASE.md`）。

安装包输出：

```text
src-tauri\target\release\bundle\nsis\Mizuki_0.2.0_x64-setup.exe
```

### 4.2 无边框窗口

- `decorations: false`，顶部透明拖拽区域，右上角最小化/最大化/关闭按钮。
- 关闭窗口默认隐藏到托盘（可在设置中改为直接退出）；托盘菜单：显示、暂停全部下载、退出。
- 单实例启动时显示并聚焦已有窗口；深浅色主题切换按钮。
- 详情面板左上角“← 返回”按钮，滚动时保持可见。

### 4.3 Bangumi 番剧数据

- 周表：`GET https://api.bgm.tv/calendar`
- 条目详情：`GET https://api.bgm.tv/v0/subjects/{subject_id}`
- 搜索：`POST https://api.bgm.tv/v0/search/subjects`（前端“搜索”页，350ms 防抖，失败时回退本地缓存匹配）
- 用户信息：`GET https://api.bgm.tv/v0/me`
- 用户收藏：`GET https://api.bgm.tv/v0/users/{username}/collections`（分页对象 `{data,total,limit,offset}`）
- 写入收藏：`POST https://api.bgm.tv/v0/users/-/collections/{subject_id}`（支持 `type` 与 `ep` 字段）
- 短评：`GET https://next.bgm.tv/p1/subjects/{subject_id}/comments`

条目详情打开时补充完整简介、`total_episodes`/`eps` 集数、评分排名、高清封面与短评。
周表卡片 `episodes == 0` 时按需补查，前端对同一 ID 的详情请求做去重合并。

### 4.4 Bangumi Access Token 同步

- 设置页可打开 `https://next.bgm.tv/demo/access-token`，粘贴 Token 后先经 `/v0/me` 验证。
- Token 只保存在 Windows 凭据管理器（service `app.mizuki.desktop` / account `bangumi-access-token`）。
- 分页导入动画收藏（每页 50 条），收藏条目资料写入 `cached_subjects`。
- 收藏状态与观看集数改动：先写本地，再经**可靠同步队列**写回 Bangumi（见 4.5）。
- 断开 Bangumi 只删除凭据，不删除本地收藏。

### 4.5 可靠同步队列（0.2.0 新增）

- `sync_queue` 表按 `subject_id` 去重，仅保留每条目最新改动；重新入队会重置退避计数。
- 后台 worker 45 秒轮询 + 入队即时唤醒；写回失败按 2^n 分钟指数退避（2 分钟起步、60 分钟封顶）。
- 设置页展示待同步数量、最近失败原因，提供“立即重试”（`retry_sync_now`）。
- 侧栏头像处显示“N 条待同步”；未连接 Token 时保持本地模式，不入队。

### 4.6 本地数据库

数据库位于 Tauri 应用数据目录的 `mizuki.sqlite3`，由 `Database::open` 自动创建和迁移。

当前表：

- `rss_feeds`（含 `rule_json` 每订阅规则）
- `rss_items`
- `downloads`（含 `source_key` 去重键）
- `local_collections`
- `cached_subjects`
- `settings`（含 `calendar_cache` 与 `app_settings`）
- `sync_queue`

### 4.7 观看进度编辑（0.2.0 新增）

- 详情页进度区提供“− / ＋1 / 全部看完”按钮与集数直接输入。
- `set_watch_progress` 命令先写本地，再带 `ep` 字段写回 Bangumi 的 `ep_status`。
- 条目尚无收藏状态时首次标记进度自动视为“在看”。

### 4.8 Mikan RSS 与匹配规则

- 可添加/验证/启停/删除订阅源；首次添加只建基线不补旧资源；刷新只处理新增 GUID。
- 每订阅规则（0.2.0 新增）：必须包含、必须排除、分辨率、字幕组、自动下载开关，行内编辑器保存到 `rss_feeds.rule_json`。
- 刷新发现的新资源按规则标记“符合规则”；开启自动下载的订阅会自动入队。
- 下载页可查看来源、发布时间与任务状态，支持手动/批量下载。

### 4.9 下载器（0.2.0 增强）

- librqbit 会话：TCP+uTP 监听、UPnP、可配置端口（0 随机）、每任务连接数（默认 256）。
- 上传/下载限速（KB/s，0 不限），保存后即时生效。
- 同时下载数上限（0 不限）：超额新任务以排队状态入会话，任务完成/暂停/删除时自动续跑。
- “下载完成后停止做种”可配置（默认开）。
- 下载目录可自定义（对新任务生效，留空为系统下载目录\Mizuki）。
- 完成任务“▶ 播放”：单视频直接打开；多视频弹出选集对话框（`download_playback_files` 按文件名排序）。
- 目录/播放经 Rust 侧 `open_local_path` 打开，自定义目录不受 opener ACL 限制。
- 应用重启自动恢复排队/下载/暂停任务。

### 4.10 设置页（0.2.0 起全部真实生效）

全部设置以 JSON 存 `settings` 表（key=`app_settings`），字段缺省自动回落默认值：

| 设置 | 生效方式 |
|---|---|
| 下载目录 | 新任务生效 |
| 下载/上传限速 | 即时 |
| BT 端口、每任务连接数 | 重启生效（UI 有标注） |
| 同时下载数 | 即时 |
| 完成后停止做种 | 即时 |
| RSS 刷新间隔（5-1440 分钟） | 即时（前端轮询间隔跟随） |
| 开机自启动 | 即时（autostart 插件） |
| 关闭窗口最小化到托盘 | 即时 |
| 收藏数据展示 | 依据是否连接 Token |

设置页另有“应用更新”区块（0.2.0）：显示当前版本，`check()` 查询 updater 端点，
发现新版本可下载安装并自动重启（`tauri-plugin-updater` + `tauri-plugin-process`）。

### 4.11 当前界面

侧边栏页面：今日（周表 + 置顶“我的追番”）、搜索、追番（五状态筛选）、RSS（订阅 + 规则 + 资源）、下载、设置。
数据等待时显示 shimmer 骨架卡片；浏览器预览显示明确“需在桌面端运行”提示，不再展示演示数据。

## 5. 关键实现约定

### 5.1 收藏状态映射

| 前端状态 | Bangumi type | 中文 |
|---|---:|---|
| `wish` | 1 | 想看 |
| `collect` | 2 | 看过 |
| `doing` | 3 | 在看 |
| `on_hold` | 4 | 搁置 |
| `dropped` | 5 | 抛弃 |

映射唯一来源：`bangumi::collection_slug` / `collection_kind`（有互逆测试）。

### 5.2 星期映射

前端 `0 周一 ... 6 周日`；`bangumi::calendar` 负责 Bangumi weekday ID 转换。
缓存收藏条目 `air_weekday` 为 -1，仅用于追番页。

### 5.3 前后端字段

Rust 模型 `#[serde(rename_all = "camelCase")]`，主要字段：
`nameCn, airWeekday, updateState, downSpeed, outputPath, lastCheckedAt, matchesRule, autoDownload, subtitleGroup`。
Bangumi 原始详情与短评按 API 原字段直传（`total_episodes, rating.score` 等）。

### 5.4 测试

`cargo test --locked` 共 26 个测试：下载源去重键、magnet tracker 合并、会话选项、
收藏分页对象/旧数组形状防回归、收藏映射互逆、周表合并（收藏注入/缓存去重/无收藏隐藏）、
数据库 upsert/缓存往返/同步队列退避、RSS 嵌套 pubDate、标题匹配规则、设置校验。

## 6. 已解决问题记录

（0.1.0 记录略，见 Git 历史 `f4158c8` 前的基线。）

0.2.0 新增：

- 收藏写回失败不再被静默忽略：sync_queue + 指数退避 + UI 重试入口。
- 观看进度可编辑并同步 Bangumi `ep_status`。
- RSS 匹配规则接入订阅 UI，规则可自动下载。
- 下载器限速/端口/连接数/并发排队/自定义目录/停止做种全部可配置。
- 播放从“最大文件”策略升级为选集对话框。
- 设置页静态展示全部改为真实落库生效。
- 移除演示数据，加载骨架 + 明确的预览/离线提示。
- CSP 从 `null` 收紧为白名单；日志审计无 Token 泄漏。
- 初始化 Git 并以 0.1.0 基线起步，逐项提交。

0.2.0 后修复：

- 今日页封面不显示：calendar 旧接口返回 `http://lain.bgm.tv` 图片地址，被 CSP 拦截；
  现在构建 Subject 与读取缓存时统一升级为 https（含回归测试）。
- 重启后已完成任务无法播放：完成任务时把主视频路径写入 `downloads.playback_path`，
  句柄不存在时播放命令回退到该记录；播放失败时前端也会兜底打开落库路径。

## 7. 已知问题与未完成项

1. 集数仍按卡片逐个补查
   - 前端已对同一 ID 去重合并，但可见卡片多时仍是多次详情请求。
   - 建议后端加详情缓存（TTL + 并发限制）或改用批量季度数据源。
2. 短评使用 Bangumi 私有 `/p1/` 接口
   - 当前可用但兼容性弱于 `/v0/`；失败时应提供跳转 Bangumi 网页的降级。
3. Updater 端点指向占位仓库
   - “应用更新”已上线（设置页检查/下载/重启），但 `plugins.updater.endpoints`
     中的 `OWNER` 需换成实际 GitHub 用户/组织，且需要先有一次带签名的 Release。
4. 观看进度写回粒度
   - `ep` 是集数计数而非具体 episode id；对使用绝对编号的条目可能与 Bangumi 章节不完全一致。
5. 文件级下载选择未实现
   - librqbit 9 的 `update_only_files` 为 crate 私有，只能在添加任务时通过 `only_files` 指定；
     当前未暴露“添加前预览种子文件并勾选”的流程。
6. `tauri-plugin-deep-link` 已配置 `mizuki://`，业务未使用。
7. `oauth-broker/` 已废弃但仍保留，应决定归档或删除。
8. 播放路径的兼容边界
   - 0.2.1 修复后：完成任务时落库主视频路径，重启后可正常播放。
   - 在修复版本之前完成的旧任务（库中无 `playback_path`）重启后仍会提示
     “任务不存在或尚未恢复”，需重新打开一次详情或重新下载。
9. 无端到端/UI 测试；Rust 侧仍有部分压缩单行风格代码（`App.tsx`、`db.rs` 个别语句）。

## 8. 建议后续开发顺序

1. 配置 GitHub 远端与 Secrets 后推一次 v0.2.1 标签，端到端验证“检查更新”真实升级。
2. 后端条目详情缓存（TTL），消除周表卡片 N+1 请求。
3. 短评接口失败时降级跳转 Bangumi 网页。
4. 为同步队列补一个真实 Bangumi 写回的集成测试（可用 staging token）。
5. 归档/删除 `oauth-broker/`，移除未用的 deep-link 配置。
6. 压缩单行代码格式化拆分（`cargo fmt` + prettier）。
7. 补端到端测试（如 WebDriver/tauri-driver）覆盖收藏与下载主流程。

## 9. 开发与构建

环境要求：Node.js 20+、Rust 1.85+、Windows WebView2、NSIS。

```powershell
cd D:\projects\Mizuki
npm install
npm run tauri dev      # 开发
npm run build          # 仅前端
cd src-tauri; cargo test --locked   # Rust 测试
cd ..; npm run tauri build          # Windows 安装包
```

发布流程（版本号、签名、CI、Authenticode）见 `RELEASE.md`。

最近一次验证结果（2026-08-28，0.2.0）：

- `npm run build`：通过
- `cargo test --locked`：26 passed
- `npm run tauri build`：通过
- 安装包：`src-tauri\target\release\bundle\nsis\Mizuki_0.2.0_x64-setup.exe`

## 10. 外部接口与参考资料

- Bangumi API：https://github.com/bangumi/api
- Bangumi OpenAPI：https://github.com/bangumi/api/blob/master/open-api/v0.yaml
- Bangumi Private API：https://bangumi.github.io/dev-docs/
- Bangumi Token 生成：https://next.bgm.tv/demo/access-token
- Mikan Project：https://mikanani.me/
- Kazumi：https://github.com/Predidit/Kazumi
- Animeko：https://github.com/open-ani/animeko
- librqbit：https://github.com/IgnisDa/rqbit
- Tauri Updater：https://v2.tauri.app/plugin/updater/

## 11. 交接检查清单

- [ ] 安装 0.2.0 NSIS 包并确认能启动。
- [ ] 检查无边框窗口拖拽、最小化、最大化和关闭到托盘（并验证设置中关闭行为切换）。
- [ ] 在设置页验证 Bangumi Access Token，确认侧栏与设置页同步状态展示。
- [ ] 修改收藏状态后断网，确认“待同步”计数增加；恢复网络或点“立即重试”后清零。
- [ ] 打开周表条目，用“＋1/全部看完”编辑进度，确认 Bangumi 端 `ep_status` 更新。
- [ ] 搜索页搜索并添加条目到收藏。
- [ ] 添加一个 Mikan RSS，配置规则（含字幕组/分辨率）并开启自动下载，确认新资源自动入队。
- [ ] 设置限速与同时下载数，添加多个任务确认排队与续跑。
- [ ] 下载完成后点击“目录”打开实际下载目录；多视频任务确认弹出选集对话框。
- [ ] 设置页“应用更新”点击“检查更新”，确认提示“已是最新版本”（无远端 Release 时可能报网络错误，属预期）。
