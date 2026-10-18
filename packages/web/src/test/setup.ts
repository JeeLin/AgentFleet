/* ===========================================================================
   vitest 环境准备
   ---------------------------------------------------------------------------
   目前只做一件事：**保护 happy-dom 全局对象里的 `inert`**。

   焦点栈依赖原生 `inert`（design-system §4.1「焦点归最内层容器，背后全部
   inert」）。happy-dom 未实现它，而原型实测的 UA 也没有真正实现它——赋值
   在真浏览器里是生效的特性开关，在这些环境里只是挂一个没人读的属性。
   这个 polyfill 让「层打开后背后 inert」成为可断言的事实，而不是一条
   永远为真的假设。
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