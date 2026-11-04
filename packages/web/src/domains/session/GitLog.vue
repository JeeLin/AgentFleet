<script setup lang="ts">
/* ===========================================================================
   GitLog —— 历史提交的只读列表（规范 §5.4 / §3.14）
   ---------------------------------------------------------------------------
   一行一提交：短 hash · 作者 · 相对时间 · 首行消息。点开取**该提交**的 diff。

   ── 为什么用 `commit` 而不是 `path` ────────────────────────────────────
   `diff` op 有三种取法，回答三个不同的问题（见
   `crates/node-core/src/connector/git.rs` 头部的表）：
   `path` = 工作区未提交改动、`staged` = 暂存区、`commit` = 某个历史提交。
   点历史提交时发 `path` 会拿到「工作区现在改了什么」——那是另一件事，
   而用户点的是那条提交。设备侧对 `commit` 用 `git show --format= --patch`，
   不是 `git diff <hash>`（后者比的是 hash 与工作区）。

   ── 空、截断、还没问过 ──────────────────────────────────────────────
   §3.12 的三个落点，混掉一个都会被读成「坏了」：
   `commits === null` 还没问过；`commits.length === 0` 是「还没有提交」
   （新建仓库的正常状态）；`truncated` 是「还有更早的，这里只给最近 N 条」。
   =========================================================================== */

import { computed } from 'vue'

import { relativeTimeLong } from '@/shared/format'

/** `log` 回帧里的一条提交。字段名与设备侧 `parse_log` 一一对应。 */
export interface GitCommit {
  /** 完整 hash（40 位）。请求该提交的 diff 时必须发这个，不能发短 hash。 */
  hash: string
  author: string
  /** ISO-8601 时间（设备侧 `%aI`）。 */
  when: string
  /** 提交消息首行。 */
  subject: string
}

const props = defineProps<{
  /** 提交列表。`null` = 还没问过（与空数组是两件事）。 */
  commits: GitCommit[] | null
  /** 还有更早的提交没列出来。 */
  truncated: boolean
  /** 正在取哪条提交的 diff（完整 hash）。用来把那一条标出来。 */
  reading: string | null
  /** 当前展示的 diff 属于哪条提交。 */
  selected: string | null
}>()

const emit = defineEmits<{
  /** 用户点了某条提交：传**完整** hash。 */
  (e: 'open', hash: string): void
}>()

/** 短 hash 取前 7 位——与 `git log --oneline` 同宽，看上去最不陌生。 */
function shortOf(hash: string): string {
  return hash.slice(0, 7)
}

/**
 * 相对时间。设备给的是 ISO-8601，换算交给 `lib/format.ts` 的同族实现——
 * 展示口径只留一份，档位边界不再各写各的。
 */
function relative(when: string): string {
  return relativeTimeLong(when)
}

/** 列表是否空到需要一句说明。`null`（没问过）不算空——那是另一句话。 */
const empty = computed(() => props.commits !== null && props.commits.length === 0)
</script>

<template>
  <div class="af-glog" data-git-log>
    <p v-if="commits === null" class="af-git__void af-t-mute" data-git-log-loading>
      正在读取提交历史…
    </p>
    <p v-else-if="empty" class="af-git__void af-t-mute" data-git-log-empty>
      这个仓库还没有提交。
    </p>
    <template v-else>
      <ul class="af-glog__list" role="listbox" aria-label="历史提交">
        <li v-for="c in commits ?? []" :key="c.hash">
          <button
            class="af-glog__row"
            type="button"
            role="option"
            :aria-selected="selected === c.hash ? 'true' : 'false'"
            :data-git-commit="c.hash"
            :title="`${c.hash}\n${c.author}\n${c.when}\n\n${c.subject}`"
            @click="emit('open', c.hash)"
          >
            <span class="af-glog__hash af-fp">{{ shortOf(c.hash) }}</span>
            <span class="af-glog__author">{{ c.author }}</span>
            <span class="af-glog__when af-t-mute">{{ relative(c.when) }}</span>
            <span class="af-glog__subject">{{ c.subject }}</span>
            <!-- 取这条提交的 diff 时给它一个可见的「在读」标记：列表不动、
                 没有反馈的话，用户会以为点击没生效。 -->
            <span v-if="reading === c.hash" class="af-glog__busy af-t-mute">读取中…</span>
          </button>
        </li>
      </ul>
      <!-- 截断要显式说出来：静默只给 30 条会让用户以为更早的提交没了。 -->
      <p v-if="truncated" class="af-glog__trunc" role="status" data-git-log-truncated>
        只显示了最近的这些提交，更早的没有列出。
      </p>
    </template>
  </div>
</template>
