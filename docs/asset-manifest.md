# 前端 bundle 的哈希表与增量更新（#99）

## 问题

`http::static` 原来对每个响应都发 `Cache-Control: no-cache`，且不带任何验证器
（无 `ETag`、无 `Last-Modified`）。`no-cache` 的语义是"可以存，但用之前必须
问一次服务器"—— 而没验证器就没法问，于是浏览器只能整份重下。

实测代价：一次冷刷新要拉 `index-vIq8uH4C.js` 738 KB + `selkies-core-yAvzPWiV.js`
535 KB + CSS 39 KB ≈ **1.3 MB**，即使这次部署一个字节都没改。协议本身没错，
错在"没有东西可以拿去对比"。

## 三层机制

只有第一层能真正省掉传输，第二层省掉重传，第三层是给**编排方**用的指纹。

| 层 | 覆盖对象 | 响应头 | 效果 |
|---|---|---|---|
| 内容哈希文件名 | `assets/*.js`、`assets/*.css` | `Cache-Control: public, max-age=31536000, immutable` | 一年内不再发请求 |
| 强验证器 | `index.html`、`jsdb/**`、`codecs/**`、图标、`manifest.json` | `Cache-Control: no-cache` + `ETag: "sha256-<hex>"` | 每次请求，命中回 `304`（几十字节） |
| 清单 | `dist/.asset-manifest.json` | `GET /api/v1/manifest` | 一次拿到全站指纹，供部署链/面板比对 |

**为什么不给所有文件都上 `immutable`**：`assets/` 的名字里带 Vite 的内容哈希
（`index-BlzAENbX.css`），改一个字节就换 URL，所以那一年的缓存永远不会错。
`index.html`、`jsdb/windows/045e-028e.json` 这些名字是固定的，同一个 URL 背后
的内容会变 —— 给它们 `immutable` 等于把更新永久钉死在用户的缓存里。

## 清单格式

构建期由 `selkies-dashboard/write-manifest.js` 生成，落在 `dist/.asset-manifest.json`
（点开头，和服务端本来就有的 PWA `manifest.json` 区分开）。

```json
{
  "schema": 1,
  "generated": "2026-11-03T03:03:53.971Z",
  "total_files": 2007,
  "total_bytes": 3778871,
  "files": {
    "assets/index-vIq8uH4C.js": {
      "size": 738508,
      "sha256": "e4ef6698c46673d4d9fc1e4a1f3b4aea9d5ab78d4dc59df5ac0a2ab09c133b4d",
      "cache": "immutable"
    },
    "index.html": { "size": 917, "sha256": "…", "cache": "revalidate" }
  },
  "units": {
    "jsdb/windows": { "files": 868, "bytes": 900715, "sha256": "…", "cache": "revalidate" }
  }
}
```

**粒度是混合的，这是刻意的**：

- 每次构建都变的东西**逐文件登记**：`index.html`、3 个 `assets/*`、4 个
  `codecs/*`、图标与 `manifest.json`、`brand/logo.svg` —— 当前共 13 条。
- `jsdb/` **按平台聚合成 5 条单元**，当前共 1994 个文件（windows 868 / linux 574 /
  android 271 / mac 262 / ios 19）。gendb 从同一份上游快照一次重写全部文件，
  客户端要么有那一份、要么没有，逐个登记只会把表撑到 ~250 KB 并且不提供任何
  额外判断力。聚合哈希的算法是：按路径排序后拼 `<path>\0<sha256>\n` 再整体哈希
  一次 —— 改名、增删、改字节都会让它变。
- 当前清单 **3,275 B**，覆盖 2007 个文件、3,778,871 B 原始字节（dist 目录本身
  是 2008 个文件 —— 多出来的那个就是清单自身）。

`cache` 字段是清单对服务端的指令，不是建议：`immutable` 只会出现在构建脚本
确认过"这个路径带内容哈希"的条目上，服务端不按扩展名猜。

## 服务端怎么用

`http::manifest` 懒加载这份表，按 `(path, mtime, len)` 失效，所以重新部署
dist 后下一个请求就生效，不用重启进程。`http::static` 在查文件缓存**之前**
先算 `(etag, cache_control)`，保证命中和未命中描述同一个文件。

`http::server` 的 `Route::Static` 分支读 `If-None-Match`：

```
请求带 If-None-Match: "sha256-e4ef66…"
  文件 etag 相同 → 304，无 body、无 Content-Length
  文件 etag 不同 → 200 + 新 body + 新 ETag
```

比较是**弱比较**：客户端把 `"x"` 回成 `W/"x"`、或在一串里列多个 tag、或用 `*`，
都算命中。空 header 和空 tag 一律不命中（否则"什么都没带"会被当成"带对了"）。

**单元内文件的 tag 怎么来**：`jsdb/windows/045e-028e.json` 不在 `files` 里，
精确查找会落空。这时按最长祖先单元回退，tag 是「单元哈希 + 路径」：

```
ETag: "150b4475…fabb38-jsdb/windows/045e-028e.json"
```

这是合法的强验证器，因为聚合哈希是对每个成员的 `path\0sha256` 求的 ——
聚合不变 ⟹ 每个成员都没变；路径参与拼接 ⟹ 同组两个文件不会共用同一个 tag。
它答不了"是哪一个变了"，但服务端要回答的只有"你手里这份还是最新的吗"。
单元匹配只在路径分隔符处生效：`jsdb/windows-extra/x.json` 不会被算进
`jsdb/windows`。

dist 里没有清单时一切回落到旧行为（`no-cache`、无验证器），不会因为老包而报错。

## `/api/v1/manifest`

原样返回构建产物，带自己的弱 `ETag`（`W/"manifest-<len>-<mtime>"`），支持 304。
单独一条路由 + 独立的 `Endpoint::Manifest`，而不是并进 `Endpoint::Static`：
静态资源是"页面要用什么"，清单是"整个包里有什么、每份哈希多少"。后者把整棵
文件树一次摊开，将来分级时可以让匿名访客拿页面却拿不到这份索引。

## 部署链怎么用

`deploy.ps1` 仍然整包上传 —— 现在的链路是"构建机 → 被控机"，一次 1.16 MB 的
zip 没必要拆成差量。清单在这一层的价值是**可核对**：解压后拿
`C:\dev\selkies-web\.asset-manifest.json` 对 source 的 `sha256` 逐条比，比
"MD5 对一个 zip" 更能说清"到底哪个文件不对"。真正的按文件差量传输要等
被控机数量上来以后再做，那时清单已经是现成的输入。

## 已知边界

- **`Connection: close` 仍然一请求一连接。** 304 省掉的是 body，不是 TCP/TLS
  握手。要再往下压得先上 keep-alive，那是另一件事。
- **没有 gzip/br 预压缩。** 静态响应直接发原始字节。1.3 MB 的 JS 压完是
  ~400 KB，收益比 304 更小但仍有，等真有需要再加。
- **`jsdb` 单元全或无。** 上游改动一个映射，整个平台目录视为变化；浏览器
  不会逐个判断，因为那些文件只在按需查手柄时一次性拉几个。
- **清单不登记自身。** 否则每写一次就改一次自己的哈希。
