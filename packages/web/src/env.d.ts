/// <reference types="vite/client" />

export {}

/**
 * 构建期注入的版本号（`{major}.{minor}.{patch}` 或带 `-pre` 的串），
 * 取值见 `vite.config.ts` 的 `define`（= `package.json` 的 `version`）。
 *
 * 左栏底部第二行要显示「v0.8.0」这种版本标注。**用构建期常量而不是运行时
 * 请求**：那一行必须离线也画得出来，多一个请求就多一种它空着的情形。
 * 测试里由 `test/setup.ts` 兜底赋值（vitest 不走 `vite build`）。
 *
 * 声明成 `globalThis` 上的属性、而不是裸 `declare const`：`test/setup.ts`
 * 要在**运行时给它赋值**，裸 `declare const` 只声明了一个模块作用域的标识符，
 * `globalThis.__APP_VERSION__ = …` 会报「不存在于 typeof globalThis」。
 */
declare global {
  var __APP_VERSION__: string
}

declare module '*.vue' {
  import type { DefineComponent } from 'vue'

  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>
  export default component
}
