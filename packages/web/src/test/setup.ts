/* ===========================================================================
   vitest 环境准备
   ---------------------------------------------------------------------------
   目前只做一件事：**保护 happy-dom 全局对象里的 `inert`**。

   焦点栈依赖原生 `inert`（规范 §4.1「焦点归最内层容器，背后全部 inert」）。
   happy-dom 未实现它，而赋值在真浏览器里是一个生效的特性开关，在这些
   环境里却只是挂一个没人读的属性——那条断言会恒真，而这个断言恒真就等于
   没有断言。这个 polyfill 让「层打开后背后 inert」成为可断言的事实。

   注意本文件只做 `inert` 一件事：`offsetParent` 在 happy-dom 里恒不为
   `null`，所以可见性判据一律用 `getComputedStyle().display` 与
   `[hidden]`（见 focus.ts / listbox.ts 的 `isVisible`）。
   =========================================================================== */

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