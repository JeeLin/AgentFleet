/**
 * 面板坞里的面板名与它们的固定顺序。
 *
 * **为什么单独一个文件**：这份枚举同时被 `PanelDock.vue`（渲染标签与「＋」
 * 候选）和 `SessionPage.vue`（键盘轮转、缺省面板）需要，而 `<script setup>`
 * **不允许 ES 导出**——常量留在组件里就只有组件自己能用，页面要复用得
 * 另抄一份，两份顺序一旦漂开，用户按 `]` 跳到的面板就与他看到的候选顺序
 * 对不上。放在 `lib/` 里两边 import 同一份。
 */

/** 坞里能放的面板。 */
export type PanelName = 'terminal' | 'files' | 'git'

/**
 * 面板的固定顺序。**这是唯一的一处**。
 *
 * 它同时决定了三件事的顺序，而这三件事必须一致：
 * 1. 「＋」候选的排列；
 * 2. `[` / `]` 的轮转顺序；
 * 3. 缺省面板（`git`）在所有面板里的位置。
 */
export const PANEL_ORDER: readonly PanelName[] = ['terminal', 'files', 'git']
