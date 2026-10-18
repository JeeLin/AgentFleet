import { fileURLToPath, URL } from 'node:url'

import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vitest/config'

// 测试专用配置：与 vite.config.ts 分开，是为了在测试里关掉 build 段的
// 产物优化（sourcemap 与压缩对测试无意义，只会让输出难读）。
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'happy-dom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.ts'],
    // xterm.js 等第三方渲染库在 happy-dom 下没有真实 canvas，
    // 面板级组件测试（子任务 13b/13c）会用自己的 mock，不依赖这个开关。
    restoreMocks: true,
  },
})
