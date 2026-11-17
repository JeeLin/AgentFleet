/* ===========================================================================
   vitest 环境准备
   ---------------------------------------------------------------------------
    ① **保护 happy-dom 全局对象里的 `inert`**。

    焦点栈依赖原生 `inert`（规范 §4.1「焦点归最内层容器，背后全部 inert」）。
    happy-dom 未实现它，而赋值在真浏览器里是一个生效的特性开关，在这些
    环境里却只是挂一个没人读的属性——那条断言会恒真，而这个断言恒真就等于
    没有断言。这个 polyfill 让「层打开后背后 inert」成为可断言的事实。

    ② **每个用例从同一份进程级状态开始**。

    happy-dom 的 `location` 是整个测试进程里共享的一个对象，而前端有不少
    行为是「跳转」：一个用例 `navigate('/sessions/s_new')` 之后，下一个用例
    的 `location.pathname` 还是 `/sessions/s_new`——断言「失败时留在
    `/sessions/new`」会因为上一条用例的路由而红，而它自己是绿的。症状是
    「单跑绿、全量红」，根因在进程级共享状态，不在被测组件里。
    `sessionStorage` 同理（它存着 tab 集合）。

    注意本文件只做这两件事：`offsetParent` 在 happy-dom 里恒不为 `null`，
    所以可见性判据一律用 `getComputedStyle().display` 与 `[hidden]`
    （见 focus.ts / listbox.ts 的 `isVisible`）。
   =========================================================================== */

import { beforeEach } from 'vitest'

/**
 * `__APP_VERSION__` 的兜底：`vite.config.ts` 用 `define` 注入它，但 **vitest 不
 * 执行 `define` 的替换**（它走自己的 transform），所以测试环境里这个全局常量
 * 根本不存在，`App.vue` 引用它会在每个挂载 `App` 的用例里抛 ReferenceError。
 *
 * 值**不带 `v` 前缀**：`vite.config.ts` 注入的是 `package.json` 的 `version`
 * （`0.8.0`），`v` 由 `App.vue` 模板自己加。这里写成 `v9.9.9-test` 会让页面上
 * 出现 `vv9.9.9-test`（实测踩过）。
 *
 * 值写死一个固定的假版本号、**不读 `package.json`**：测试要判断的是「这一行
 * 渲染了版本号」，不是「版本号等于 package.json 里的值」——后者会让每次发版
 * 都改一次测试。真值的正确性由 `vite.config.ts` 那一处保证。
 */
globalThis.__APP_VERSION__ = '9.9.9-test'

if (!('inert' in HTMLElement.prototype)) {
  Object.defineProperty(HTMLElement.prototype, 'inert', {
    configurable: true,
    get(this: HTMLElement): boolean {
      return this.hasAttribute('inert')
    },
    set(this: HTMLElement, value: boolean) {
      if (value) this.setAttribute('inert', '')
      else this.removeAttribute('inert')
    },
  })
}

beforeEach(() => {
  // 每个用例从同一个地址开始：happy-dom 的 `location` 是整个测试进程共享的
  // 一个对象，一个用例里 `navigate('/sessions/s_new')` 之后，下一个用例的
  // `location.pathname` 还是 `/sessions/s_new`——断言「失败时留在
  // `/sessions/new`」会因为上一条用例的路由而红，而它自己是绿的。症状是
  // 「单跑绿、全量红」，根因在进程级共享状态，不在被测组件里。
  window.history.replaceState(null, '', '/sessions')
  // sessionStorage 也在同一批进程级共享状态里：不清它的话，上一个用例存下
  // 的 tab 集合会漏进下一个用例。
  window.sessionStorage.clear()
})