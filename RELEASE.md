# Mizuki 发布流程

> 适用版本：0.2.0+；目标平台：Windows x64（NSIS 安装包 + Updater 升级包）

## 1. 发布前检查

```powershell
cd D:\projects\Mizuki
npm run build          # 前端 tsc + vite
cd src-tauri; cargo test --locked   # Rust 单元/集成测试
cd ..; npm run tauri build          # 本地完整构建
```

安装包输出：`src-tauri/target/release/bundle/nsis/Mizuki_<版本>_x64-setup.exe`

## 2. 版本号

三处必须一致（`package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`）。
发布前确认均已更新，例如 `0.2.0 -> 0.3.0`。

## 3. Updater 签名密钥

- 私钥：`.keys/mizuki.key`（已加入 `.gitignore`，**绝不能提交或外传**）
- 公钥：已写入 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`
- 当前私钥为**空密码**生成，仅供本机与 CI 使用；如需更严格保管，重新生成并同步替换公钥：

```powershell
npm run tauri signer generate -- -w .keys/mizuki.key --password "<新密码>"
# 用新的 mizuki.key.pub 内容替换 tauri.conf.json 中的 pubkey
```

丢失私钥后已发布的用户将无法自动升级，只能重新分发安装包。

## 4. GitHub Release（自动）

1. 把仓库推到 GitHub（当前仓库存放在 D:\projects\Mizuki，尚未配置远端时先 `git remote add origin ...`）。
2. 仓库 Settings → Secrets 添加：
   - `TAURI_SIGNING_PRIVATE_KEY`：`.keys/mizuki.key` 文件内容
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：留空（空密码）
3. 修改 `src-tauri/tauri.conf.json` 中 `plugins.updater.endpoints` 的 `OWNER` 为实际 GitHub 用户/组织名。
4. 发布：

```powershell
git tag v0.2.0
git push origin v0.2.0
```

`.github/workflows/release.yml` 会在 windows-latest 上构建并创建 Release 草稿，
tauri-action 自动生成 `latest.json`（Updater 索引）与 `.sig` 签名文件。人工检查草稿后发布。

## 5. 代码签名（Authenticode，可选，需外部采购）

安装包默认未做 Authenticode 签名，SmartScreen 会提示未知发布者。取得代码签名证书后：

1. 将证书（含私钥的 `.pfx`）导入证书存储，或使用 EV USB 令牌。
2. 在 `tauri.conf.json` 的 `bundle` 中补充：

```json
"windows": {
  "certificateThumbprint": "<证书指纹>",
  "digestAlgorithm": "sha256",
  "timestampUrl": "http://timestamp.digicert.com"
}
```

3. CI 环境可改用 `signtool sign /fd SHA256 /tr <RFC3161时间戳> /f <pfx>` 后处理签名。

## 6. 用户升级路径

- 全新安装：`Mizuki_<版本>_x64-setup.exe`。
- 已安装用户：设置页 →“应用更新”→“检查更新”，发现新版本后点击“下载并重启”
  （走 `latest.json` 索引与 minisign 签名校验，签名不符会拒绝安装）。

## 7. 常见问题

- **构建时 updater 报 pubkey 无效**：`tauri.conf.json` 的 pubkey 必须是 `.key.pub` 文件的完整一行内容。
- **CI 中签名失败**：确认私钥 Secret 没有多余换行/空格。
- **NSIS 包体积异常**：检查 `bundle.icon` 列表未被替换为超大图。
