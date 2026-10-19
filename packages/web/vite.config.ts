import { fileURLToPath, URL } from 'node:url'

import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

// 四个门禁共享同一份配置：`vite build` 与 `vitest run` 都读它，
// 避免「构建能过、测试跑不起来」这种只在门禁上才暴露的分裂。
export default defineConfig({
  plugins: [vue()],
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
  // 开发时页面跑在 5173、Hub 在 8080，于是 `fetch('/api/…')` 会打到 Vite
  // 自己身上。代理把 `/api` 与 `/ws` 交回 Hub，前端代码里**一个绝对地址
  // 都不用写**——`http.ts` 用的就是同源相对路径，改动被这份配置吃掉。
  //
  // 生产不走这条：子任务 6 由 Hub 自己托管 `dist/`，同源相对路径照样成立。
  // 所以「开发代理」与「生产自托管」是同一份前端代码的两种后端，
  // 而不是两种前端。
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: false,
      },
      '/ws': {
        target: 'ws://127.0.0.1:8080',
        ws: true,
      },
    },
  },
})
