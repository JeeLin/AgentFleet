/* ===========================================================================
   vitest 环境准备
   ---------------------------------------------------------------------------
    ① **保护 happy-dom 全局对象里的 `inert`**。

    焦点栈依赖原生 `inert`（规范 §4.1「焦点归最内层容器，背后全部 inert」）。
    happy-dom 未实现它，而赋值在真浏览器里是一个生效的特性开关，在这些
    环境里却只是挂一个没人读的属性——那条断言会恒真，而这个断言恒真就等于
    没有断言。这个 polyfill 让「层打开后背后 inert」成为可断言的事实。

    ② **每个用例从同一个地址开始**。

    happy-dom 的 `location` 是整个测试进程里共享的一个对象，而前端有不少
    行为是「跳转」：一个用例 `navigate('/sessions/s_new')` 之后，下一个用例
    的 `location.pathname` 还是 `/sessions/s_new`——断言「失败时留在
    `/sessions/new`」会因为上一条用例的路由而红，而它自己是绿的。症状是
    「单跑绿、全量红」，根因在进程级共享状态，不在被测组件里。

    注意本文件只做这两件事：`offsetParent` 在 happy-dom 里恒不为 `null`，
    所以可见性判据一律用 `getComputedStyle().display` 与 `[hidden]`
    （见 focus.ts / listbox.ts 的 `isVisible`）。
   =========================================================================== */

import { beforeEach } from 'vitest'

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
  window.history.replaceState(null, '', '/sessions')
  window.sessionStorage.clear()
})

/*
  每个用例从同一个地址开始。

  happy-dom 的 `location` 是整个测试进程共享的一个对象：一个用例里
  `navigate('/sessions/s_new')` 之后，下一个用例的 `location.pathname`
  还是 `/sessions/s_new`——于是断言「失败时留在 /sessions/new」会因为上一条
  用例的路由而红，而那条用例自己是绿的。症状是「单跑绿、全量红」。
*/
beforeEach(() => {
  window.history.replaceState(null, '', '/sessions')
})