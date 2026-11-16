import { createApp } from 'vue'

import App from './App.vue'

// 两层全局样式：`styles/` 是设计系统（token 与组件层），`page-chrome.css`
// 是页面层那几个跨页面复用的类。顺序无关——两者不共用选择器。
import './styles/index.css'
import './page-chrome.css'
// 主题机制在 import 时就把 `<html data-theme>` 落到当前该用的那套上，
// 必须早于 `mount`，否则首屏会先闪一下默认配色（原型 `swatch.js:96` 同理）。
import './styles/theme'

createApp(App).mount('#app')
