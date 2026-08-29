# Mizuki

Mizuki 是面向 Windows 的 Bangumi 动漫追踪与 Mikan RSS 下载管理器，使用 Tauri 2、Rust、React 和 TypeScript。

## 开发

要求：Node.js 20+、Rust 1.85+、Windows WebView2。

```powershell
npm install
npm run tauri dev
```

Rust 测试：`cd src-tauri && cargo test --locked`；格式化：Rust 用 `cargo fmt`，前端用 `npm run format`。

## 本地追番模式

Mizuki 采用与 Kazumi 相近的本地管理方式，不要求登录 Bangumi，也不需要创建 OAuth 应用。番剧资料、评分和公开评论取自 Bangumi；“想看 / 在看 / 看过 / 搁置 / 抛弃”等个人状态默认保存在本机 SQLite 数据库中。

需要跨端同步时，可在设置页打开 Bangumi 的 Access Token 生成页，粘贴 Personal Access Token。Mizuki 会先通过 `/v0/me` 验证令牌，再将其保存到 Windows 凭据管理器；令牌不会写入 SQLite、日志或前端配置。连接后每次启动 Mizuki 都会自动拉取一次 Bangumi 收藏，也可以在设置页手动同步；收藏状态与观看集数改动会通过可靠的同步队列（失败自动指数退避重试）写回 Bangumi。

## RSS 与下载

- 在番剧详情页一键订阅：Mizuki 按条目生成 Mikan 单番 RSS，新集发布时按规则自动下载；追番卡片会显示“有更新 / 下载中 / 已下载”。
- 支持为每个 Mikan RSS 订阅配置匹配规则（必须包含 / 排除 / 分辨率 / 字幕组）与自动下载开关。
- 内置 librqbit 下载器：限速、端口、连接数、同时下载数、完成后停止做种、自定义下载目录均可配置；下载完成时发送系统通知。
- 下载完成的任务可直接播放；多视频合集会弹出选集对话框。

## 构建

```powershell
npm run build
npm run tauri build
```

Windows 安装程序输出于 `src-tauri/target/release/bundle/nsis/`。发布与升级签名流程见 [RELEASE.md](RELEASE.md)，项目结构与细节见 [HANDOFF.md](HANDOFF.md)。

## 数据与隐私

- 本地数据库位于 Tauri 应用数据目录下的 `mizuki.sqlite3`。
- 追番收藏与观看进度只保存在本机；同步失败会进入本地队列重试，不丢数据。
- 番剧封面与条目详情会缓存到应用数据目录（`img_cache/` 与数据库），已缓存的封面直接从本地加载，离线时详情页仍可回退展示。
- Mikan 只接收用户粘贴的 RSS 地址，不保存 Mikan 密码。
- RSS 新资源默认只发现不下载；开启自动下载的订阅按规则处理新增条目。
- 用户应确保下载内容符合所在地法律与资源授权。

## 许可

Mizuki 以 [MIT](LICENSE) 协议开源。
