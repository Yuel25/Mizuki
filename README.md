# Mizuki

Mizuki 是面向 Windows 的 Bangumi 动漫追踪与 Mikan RSS 下载管理器，使用 Tauri 2、Rust、React 和 TypeScript。

## 开发

要求：Node.js 20+、Rust 1.85+、Windows WebView2。

```powershell
npm install
npm run tauri dev
```

前端可单独通过 `npm run dev` 预览；无法连接 Tauri commands 时会显示内置演示数据。

## 本地追番模式

Mizuki 采用与 Kazumi 相近的本地管理方式，不要求登录 Bangumi，也不需要创建 OAuth 应用。番剧资料、评分和公开评论取自 Bangumi；“想看 / 在看 / 看过 / 搁置 / 抛弃”等个人状态默认保存在本机 SQLite 数据库中。

需要跨端同步时，可在设置页打开 Bangumi 的 Access Token 生成页，粘贴 Personal Access Token。Mizuki 会先通过 `/v0/me` 验证令牌，再将其保存到 Windows 凭据管理器；令牌不会写入 SQLite、日志或前端配置。连接后可以导入 Bangumi 收藏，后续收藏状态也会同步写回 Bangumi。

## 构建

```powershell
npm run build
npm run tauri build
```

Windows 安装程序输出于 `src-tauri/target/release/bundle/nsis/`。

## 数据与隐私

- 本地数据库位于 Tauri 应用数据目录下的 `mizuki.sqlite3`。
- 追番收藏与观看进度只保存在本机。
- Mikan 只接收用户粘贴的 RSS 地址，不保存 Mikan 密码。
- RSS 新资源默认自动匹配；首次添加订阅不会补下历史条目。
- 用户应确保下载内容符合所在地法律与资源授权。
