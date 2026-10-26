# 前端第三方库(vendored)

按 D4(零构建链、零 npm)直接入库,由 `include_str!` 内嵌进二进制;升级时手工替换并更新下表。

| 文件 | 版本 | 许可 | 来源 |
|---|---|---|---|
| `marked.min.js` | 12.0.2 | MIT | <https://cdn.jsdelivr.net/npm/marked@12.0.2/marked.min.js> |
| `purify.min.js` | 3.1.6 | Apache-2.0 OR MPL-2.0 | <https://cdn.jsdelivr.net/npm/dompurify@3.1.6/dist/purify.min.js> |
| `highlight.min.js` | 11.9.0 | BSD-3-Clause | <https://cdn.jsdelivr.net/npm/@highlightjs/cdn-assets@11.9.0/highlight.min.js> |
| `hljs-github-dark.min.css` | 11.9.0 | BSD-3-Clause | 同上包 `styles/github-dark.min.css` |
| `hljs-github.min.css` | 11.9.0 | BSD-3-Clause | 同上包 `styles/github.min.css` |
| `ansi_up.js` | 6.0.6 | MIT | <https://cdn.jsdelivr.net/npm/ansi_up@6.0.6/ansi_up.js>(ES module 构建) |

许可原文:`LICENSE-marked.md`、`LICENSE-dompurify.txt`、`LICENSE-highlightjs.txt`、`LICENSE-ansi_up.txt`。

## 校验和(SHA-256)

```
554a7d9ca4f3721db1f14941f92dc75b254f57d4b7bffeb84eea1174aa160780  ansi_up.js
837a6fa5b0c736b52bbde2b2b6190f305da3fc9ed41681db5321507057b5c846  highlight.min.js
15fabce5b65898b32b03f5ed25e9f891a729ad4c0d6d877110a7744aa847a894  marked.min.js
c0845096a7c4a6741f362ac506c94c1c7d27dc603bcc1bf64a587f76f2dbe3a1  purify.min.js
9f208d022102b1d0c7aebfecd8e42ca7997d5de636649d2b31ea63093d809019  hljs-github-dark.min.css
3a9a5def8b9c311e5ae43abde85c63133185eed4f0d9f67fea4b00a8308cf066  hljs-github.min.css
```

## 使用约定

- 页面只从 `'self'` 加载脚本(CSP 见 `src/server.rs`),没有任何 CDN 依赖。
- **模型与工具输出一律视为不可信**:markdown 先 `marked` 再 `DOMPurify`;工具输出先 `ansi_up`
  转义再 `DOMPurify`。页面自身不用 `innerHTML` 拼接未净化内容。
- `img` / `iframe` / `form` / `style` 被禁:远程图片是追踪面,不外链加载。
- 升级库时必须重跑 `tests/ws_smoke.rs` 的资源断言与真浏览器冒烟。
