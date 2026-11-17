import { fileURLToPath, URL } from 'node:url'
import { readFileSync } from 'node:fs'

import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

import { devProxy } from './dev-proxy.config'

/** `package.json` 的版本号，供下面的 `define` 注入（见那里的注释）。 */
const pkg = JSON.parse(readFileSync(fileURLToPath(new URL('./package.json', import.meta.url)), 'utf8')) as {
  version: string
}

// 四个门禁共享同一份配置：`vite build` 与 `vitest run` 都读它，
// 避免「构建能过、测试跑不起来」这种只在门禁上才暴露的分裂。
export default defineConfig({
  plugins: [vue()],
  // 左栏底部要显示版本号（「v0.8.0」那种）。**构建期替换成字面量**，而不是
  // 运行时请求 `/version`：那一行必须离线也画得出来，多一个请求就多一种它空着
  // 的情形。值取自 `package.json` 的 `version`——单一真相源，发版只改一处。
  //
  // 注意：`vite build` 与 `vitest` 都读这份 config，但 **`vitest` 不会执行
  // `define` 的替换**（它走自己的 transform），所以 `test/setup.ts` 里另有一份
  // 兜底赋值。两处要一起改，测试 `App.test.ts` 看守着这件事。
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  build: {
    // Hub 侧用 include_dir! 把产物嵌进二进制（子任务 6），所以产物体积
    // 直接进发布件；这里只做默认压缩，不再手动拆包。
    outDir: 'dist',
    emptyOutDir: true,
  },
  // **只在 `vite dev` 下生效**（`vite build` 不读 server.proxy）。
  //
  // 开发时页面跑在 3002、Hub 在 8080，于是 `fetch('/api/…')` 会打到 Vite
  // 自己身上。代理把后端路径交回 Hub，前端代码里**一个绝对地址都不用写**
  // ——`http.ts` 用的就是同源相对路径，改动被这份配置吃掉。
  //
  // 转发表见 `dev-proxy.config.ts`：那里能写注释说明「哪几条要代理、为什么
  // `/login` 的 GET 不能一起转」，也方便 `src/lib/dev-proxy.test.ts` 直接读。
  //
  // 生产不走这条：子任务 6 由 Hub 自己托管 `dist/`，同源相对路径照样成立。
  // 所以「开发代理」与「生产自托管」是同一份前端代码的两种后端，
  // 而不是两种前端。
  server: {
    // 绑 0.0.0.0 便于从别的机器（含 Tailscale）打开开发页面。
    // Hub 仍只监听 127.0.0.1:8080（约束 1），代理是同一台机器上的回环跳转。
    host: '0.0.0.0',
    port: 3002,
    strictPort: true,
    allowedHosts: true,
    proxy: devProxy,
  },
  // **不要在这里加 `test` 键**：vitest 读的是 `vitest.config.ts`（同目录），
  // 本文件里的 `test` 段从来不会被读取——v0.8.1 G18 最初就把覆盖率阈值写在
  // 这里，实测阈值静默不生效、`82.56%` 的行覆盖率照样 `EXIT=0`。
})
