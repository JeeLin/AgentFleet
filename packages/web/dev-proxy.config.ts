import type { ProxyOptions } from 'vite'

/**
 * `vite dev` 的后端转发表。
 *
 * 单独成文件而不是写死在 `vite.config.ts` 里，是为了让它能被单测读：
 * 漏一条前缀的症状是**开发时那条接口直接 404**，而生产由 Hub 同源托管
 * 照样能跑，`bun run build` 与 `cargo test` 都不会红。
 *
 * **哪些路径要代理，是按「它是不是后端接口」判断，而不是按「它在不在
 * `/api` 下」**：`POST /login` 与 `POST /setup` 同属后端端点
 * （`crates/hub-server/src/lib.rs` 的 `build_app`），只是路径在 `/api/*` 之外。
 */

export const HUB_HTTP = 'http://127.0.0.1:8080'
export const HUB_WS = 'ws://127.0.0.1:8080'

/**
 * `GET` / `HEAD` 留在 Vite（页面），其余方法交给 Hub（端点）。
 *
 * 返回 `req.url` 本身：Vite 的 `bypass` 收到字符串就当作「改写路径后走自己
 * 的中间件链」，于是 `/login` 落到 html fallback，出的是 SPA 外壳。
 */
const pagesStayLocal: NonNullable<ProxyOptions['bypass']> = (req) =>
  req.method === 'GET' || req.method === 'HEAD' ? (req.url ?? '/') : undefined

export const devProxy: Record<string, string | ProxyOptions> = {
  '/api': {
    target: HUB_HTTP,
    changeOrigin: false,
  },
  '/healthz': {
    target: HUB_HTTP,
    changeOrigin: false,
  },
  '/ws': {
    target: HUB_WS,
    ws: true,
  },
  // `/login` 与 `/setup` 是**同路径两种东西**：POST 是端点（归 Hub），
  // GET 是页面（归 Vite 的 SPA 外壳 + 热更新）。整条代理会把 GET 也送给
  // Hub，开发时刷新登录页拿到的就是上一次 `vite build` 的 `dist/`——
  // 页面能开，但改的代码一行都不生效。
  '/login': {
    target: HUB_HTTP,
    changeOrigin: false,
    bypass: pagesStayLocal,
  },
  '/setup': {
    target: HUB_HTTP,
    changeOrigin: false,
    bypass: pagesStayLocal,
  },
}
