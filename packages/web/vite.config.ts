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
})
