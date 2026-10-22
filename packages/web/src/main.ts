import { createApp } from 'vue'

import App from './App.vue'

// 两层全局样式：`styles/` 是设计系统（token 与组件层），`page-chrome.css`
// 是页面层那几个跨页面复用的类。顺序无关——两者不共用选择器。
import './styles/index.css'
import './page-chrome.css'

createApp(App).mount('#app')
