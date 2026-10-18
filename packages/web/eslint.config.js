// @ts-check
import eslintConfigPrettier from 'eslint-config-prettier'
import pluginVue from 'eslint-plugin-vue'
import vueTsConfig from '@vue/eslint-config-typescript'

export default [
  {
    name: 'agentfleet/ignores',
    ignores: ['dist/**', 'node_modules/**'],
  },
  ...pluginVue.configs['flat/essential'],
  ...vueTsConfig(),
  eslintConfigPrettier,
  {
    name: 'agentfleet/rules',
    files: ['**/*.ts', '**/*.vue'],
    rules: {
      // 生产代码不许吞错：一个裸 catch {} 会把断线、越界、保存冲突全变成
      // 「静默无反应」，而这几种恰恰是 design-system §7 要求可见的。
      'no-empty': ['error', { allowEmptyCatch: false }],
      // 焦点栈与键位表是全站共享的可变状态，禁 var 之外再禁一次
      // 函数级提升造成的顺序依赖（keynav.ts 的 handler 注册顺序有讲究）。
      'no-var': 'error',
      eqeqeq: ['error', 'smart'],
      'prefer-const': 'error',
      'vue/multi-word-component-names': 'error',
    },
  },
]
