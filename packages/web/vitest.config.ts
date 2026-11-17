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
    /* 前端覆盖率门禁（v0.8.1 G18）。

       **为什么不追 98%**：AGENTS.md 那条「行覆盖率 ≥98%」是
       `mise run cov-gate` 的判据，跑的是 `cargo llvm-cov`——**只对 Rust 生效**。
       前端此前根本没有覆盖率门禁（`@vitest/coverage-v8` 连装都没装，
       `vitest run --coverage` 直接 `MISSING DEPENDENCY`），于是里程碑里
       反复引用的「98.11%」无法复核，也没人发现它说的是后端。

       阈值取**实测基线**而不是理想值：2026-11-18 首次测得
       Stmts/Lines **96.33%**、Branch 87.27%、Funcs 88.88%。
       取整向下定 96%，作用是**防劣化**（新增没人测的代码会被拦住），
       不是宣称「已达 98%」。想提高就提高这条线，且必须同时补测试。

       **位置很重要**：这份配置在 `vitest.config.ts` 里，不是
       `vite.config.ts`——后者虽有 `test` 键但根本不被 vitest 读取
       （本项最初写在那儿，阈值静默不生效，实测才发现的）。 */
    coverage: {
      provider: 'v8',
      include: ['src/**/*.{ts,vue}'],
      exclude: [
        'src/**/*.test.ts',
        // 纯类型与声明，没有可执行语句
        'src/env.d.ts',
        'src/**/*.d.ts',
        // 入口装配：`main.ts` 只做 mount，其行为由 App.test.ts 覆盖
        'src/main.ts',
      ],
      reporter: ['text-summary', 'text'],
      thresholds: {
        lines: 96,
        statements: 96,
        // 分支/函数覆盖率明显低于行覆盖率（大量 `v-if` 的未走分支），
        // 单列更低的线，免得它们把行阈值一起拖下来后失去意义。
        branches: 86,
        functions: 88,
      },
    },
  },
})
