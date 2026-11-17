<script setup lang="ts">
/* ===========================================================================
   DevicesPage —— 设备与 Agent（规范 §5.1 / §5.3）
   ---------------------------------------------------------------------------
   这一页只有一个会话入口：「用它开会话」，而它**在 v0.8.0 恒禁用**
   （拿不到项目上下文，`/sessions/new` 也不是真路由；见
   `DeviceTable.vue` 文件头）。**没有面板 / 工作区入口**：
   终端与文件绑定会话的工作区 `(node_id, path)`，生命周期随会话（约束 3）。

   状态只看心跳年龄：45 秒内是 `reconnecting`（Hub 内存推导值，带「推」徽标），
   超过 45 秒判 `offline`。这个判据来自 `security-and-lifecycle.md` §5，不是
   页面自己定的。

   ── 为什么「Agent 目录」并到了这里 ────────────────────────────────
   用户裁定侧栏只留**会话 / 设备 / 设置**三项，`/agents` 路由删除。理由不是
   「页数少一点」：这两块看的是**同一个对象**——「这台机器上有什么」。设备行
   说机器在不在线，Agent 目录说它上面装了哪些 agent；分两条路由时用户要在两个
   地址之间跳，而两者拉的是同一批设备、同一份心跳事实。

   分段用**查询参数** `?view=agents` 表达而不是第二条路由：分段切换是视图切换
   （DECISIONS D10），不该进历史记录栈——否则「后退」变成在分段之间来回跳。
   地址栏仍然跟着走，所以「打开 Agent 目录」这件事还能深链接（旧 `/agents`
   书签由 `App.vue` 重定向到这里，见那里的 `LEGACY_PATHS`）。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import AgentCatalog from '@/domains/agent/AgentCatalog.vue'
import PageHead from '@/domains/shared/components/PageHead.vue'
import DeviceTable from '@/domains/device/DeviceTable.vue'
import type { DeviceSummary } from '@/domains/device/api'
import {
  deleteDevice,
  DOWNLOAD_PLATFORM_OPTIONS,
  downloadAgentBinary,
  guessDownloadPlatform,
  listDevices,
  parsePlatformKey,
  platformKey,
} from '@/domains/device/api'
import { describeReadFailure } from '@/shared/http'
import { percent, relativeTime } from '@/shared/format'
import { navigate, route } from '@/router'

/**
 * 当前分段。**从 `route` 读、不另外存一份**：地址栏是真相源，存一份本地
 * 状态会让「前进/后退」与页面显示的段落不一致，而症状是「地址写着 agents、
 * 屏幕上却是设备表」。
 */
type Segment = 'devices' | 'agents'
const segment = computed<Segment>(() =>
  route.value.query.get('view') === 'agents' ? 'agents' : 'devices',
)

/** 目录组件的句柄：顶部那个「刷新」要同时刷当前分段。 */
const catalog = ref<InstanceType<typeof AgentCatalog> | null>(null)

function selectSegment(next: Segment): void {
  // 视图切换：`replaceState` 不进历史栈（见文件头注释）。用 `navigate`
  // 的 `replace` 而不是 `history` 直改，好让 `route` 这个 ref 跟着更新。
  const to = next === 'agents' ? '/devices?view=agents' : '/devices'
  navigate(to, { replace: true })
}

/** 顶部刷新：只刷当前分段，不替用户做多余请求。 */
function refreshCurrent(): void {
  if (segment.value === 'agents') catalog.value?.load()
  else void load()
}

const devices = ref<DeviceSummary[]>([])
const loading = ref(true)
const error = ref<string | null>(null)
const inspected = ref<DeviceSummary | null>(null)
/** 移除失败的理由。与 `error` 分开：读失败会顶掉整张表，写失败只该提示一条。 */
const removeError = ref<string | null>(null)

const online = computed(() => devices.value.filter((d) => d.status === 'online'))
const reconnecting = computed(() => devices.value.filter((d) => d.status === 'reconnecting'))
const offline = computed(() => devices.value.filter((d) => d.status === 'offline'))

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    devices.value = await listDevices()
  } catch (err) {
    error.value = describeReadFailure(err, '设备列表')
  } finally {
    loading.value = false
  }
}

/** 行 ⏎：把详情显示在表格上方的一块内联区，而不是模态。
 *  模态会锁焦点，而「扫一眼设备」不该打断用户本来在做的事。 */
function inspect(device: DeviceSummary): void {
  inspected.value = inspected.value?.id === device.id ? null : device
}

/**
 * 从这一台 Hub 下载 `afcli`。
 *
 * 空态里那条「在设备上跑 afcli run」，前提是那台设备上**已经有** afcli。
 * 没有的人在离线/内网环境里够不到 GitHub，于是空态就成了一句空话——所以
 * 这里给它一条实际的路。
 *
 * # 为什么要给用户一个平台下拉
 *
 * 原来这里把 `detectDownloadPlatform(navigator.userAgent, …)` 的推断结果直接
 * 拼进 `?os=&arch=`，也就是**只能**下到「浏览器这台机器」的版本。用户在
 * Windows 上想给一台 Linux 服务器准备 afcli、或在 x86_64 Mac 上装的是
 * Apple Silicon 版，都没有任何入口能改——而后端的 `?os=&arch=` 早就是闭集、
 * 镜像里也备了别的平台，能力在、只是没有按钮。所以入口从「一个按钮」变成
 * 「平台下拉 + 下载按钮」。
 *
 * 默认值由 `guessDownloadPlatform` 猜一个；**猜错不影响功能**（用户能改），
 * 而**猜不到就不猜**——猜成 Linux 会让 Windows 用户以为自己选对了，拿到一个
 * 跑不起来的文件。认不出来时下拉框空着、下载按钮禁用、旁边写「请选择平台」。
 *
 * 三种结果分别处置，**不能合并成一条「下载失败」**：
 * - 成功 → 触发浏览器下载；
 * - `agent_binary_unavailable` → 裸机跑 afhub 时磁盘上没有 afcli，这是
 *   **正常部署形态**，提示应说「这台 Hub 没配」，不是「坏了」；
 * - 其它（网络错误 / 404 / 400）→ 行内红字「下载失败，请重试」，不弹 alert。
 */
const downloadState = ref<'idle' | 'busy'>('idle')
const downloadError = ref<string | null>(null)

/**
 * 下拉框选中值（`windows/arm64` 这种形态，理由见 `api.ts` 的 `platformKey`）。
 * 空串 = 还没选——这时按钮必须禁用，因为不带 `os`/`arch` 请求时后端会回落到
 * **Hub 自己**的平台（Linux），Windows/macOS 用户会拿到一个跑不起来的文件。
 *
 * 默认值在 `setup` 里就地算一次（不放进 `onMounted`）：骨架屏期间控件还没
 * 渲染，晚一步赋值会让第一帧闪一个「已选中」的下拉框。
 */
const downloadPlatformKey = ref<string>(
  (() => {
    const guessed = guessDownloadPlatform(navigator.userAgent, navigator.platform)
    return guessed ? platformKey(guessed) : ''
  })(),
)

/** 选中的平台；`null` = 没选/认不出的值 → 下载按钮禁用。 */
const selectedPlatform = computed(() => parsePlatformKey(downloadPlatformKey.value))

async function downloadCli(): Promise<void> {
  const platform = selectedPlatform.value
  // 按钮 `disabled` 已经拦了一层，这里再拦一次：禁用态是靠模板属性实现的，
  // 而 `@click` 仍可被键盘/程序触发。没有平台就发请求，等于让后端按它自己的
  // 平台回落——那正是这个缺陷要消灭的行为。
  if (!platform) return
  downloadState.value = 'busy'
  downloadError.value = null
  try {
    // 把**用户选的**平台交给 Hub，而不是浏览器推断的那一个。
    const { blob, filename } = await downloadAgentBinary({
      os: platform.os,
      arch: platform.arch,
    })
    // `URL.createObjectURL` + 一个隐藏 `<a>`：这是唯一在主流浏览器上都能
    // 触发下载、且不必让用户先存到别处的做法。用完立刻 revoke——不释放会在
    // 页面存活期间一直占着这份 blob。
    const url = URL.createObjectURL(blob)
    try {
      const a = document.createElement('a')
      a.href = url
      // 用服务端给的文件名：它带平台，用户同时有多台异构机器时分得清。
      a.download = filename
      document.body.appendChild(a)
      a.click()
      a.remove()
    } finally {
      URL.revokeObjectURL(url)
    }
  } catch (err) {
    const code = (err as { code?: string } | null)?.code
    downloadError.value =
      code === 'agent_binary_unavailable'
        ? '这台 Hub 没有可下发的 afcli：直接跑 afhub（不经容器）时它是外挂文件，需要在 Hub 机器上配 AFCLI_BINARY_PATH 指向它。'
        : // 网络错误、404、400 一律这一句：用户在这里能做的只有「再点一次」或
          // 「换一个平台」。把 `bad_request` 之类的 code 摊给用户看，既不能指导
          // 下一步，又让人以为 Hub 坏了。
          '下载失败，请重试'
  } finally {
    downloadState.value = 'idle'
  }
}

/**
 * 「用它开会话」的落点。**v0.8.0 里它不可达，函数保留但不再跳转**。
 *
 * v0.7.0 起会话是项目下的实体（`/project/:id/session/:sessionId`），而设备页
 * 是**管理面**（「管理从设备进」），本身不带项目上下文——所以没有一条能带着
 * 设备直达新建页的地址。`/sessions/new` 不在 `router.ts` 的 `ROUTES` 里
 * （`matchRoute` 判 `not-found`，`LEGACY_PATHS` 把它 replace 回总览），
 * 以前跳过去的结果是「静默回首页、`?device=` 被丢掉」。
 *
 * 现在按钮在 `DeviceTable.vue` 里恒 `disabled` 并写明理由，所以这个函数
 * **在正常路径上不会被调用**（`@click` 在禁用按钮上不触发）。保留它是为了
 * 让「入口为什么不可用」有一个可读的落点，以及 v0.8.2「从设备开工」接上
 * 真正入口时不必重新找位置。
 */
function createSession(device: DeviceSummary): void {
  // 不跳转：没有可达目的地。留着参数是为了签名与 `onCreate` 一致。
  void device
}

/**
 * `AgentCatalog` 的 `start-session` 落点：同样**不跳转**。
 *
 * 原因与 `createSession` 逐字相同（`/sessions/new` 不是路由）。两处放在一起
 * 是为了让「这两个入口在 v0.8.0 都不可用」在代码里是一眼可见的一件事，
 * 而不是散在模板里两处各写一遍跳转。
 */
function noSessionEntry(): void {
  // 见 `createSession` 的文档：v0.8.0 没有可达的会话入口。
}

/**
 * 移除一台设备。
 *
 * # 成功之后**只**去掉那一行
 *
 * 会话与轨迹留在 Hub 上，所以这里不碰任何会话状态、不发 `navigate`——去动
 * 别的东西会把「会话还在」这条事实在页面上做反。列表重新拉一次是因为设备行
 * 从 Hub 上没了，本地这一份已经是旧事实。
 *
 * # 404 当成成功
 *
 * 重复点击、或另一个标签页先删了，都会回 `not_found`。目标状态已经达成，
 * 给它画一条红字只会让用户去重试一件已经做完的事。其余错误照常报出。
 */
async function remove(device: DeviceSummary): Promise<void> {
  removeError.value = null
  try {
    await deleteDevice(device.id)
  } catch (err) {
    const code = (err as { code?: string } | null)?.code
    if (code !== 'not_found') {
      removeError.value = `没能移除「${device.name}」：${describeReadFailure(err, '设备')}`
      return
    }
  }
  if (inspected.value?.id === device.id) inspected.value = null
  await load()
}
</script>

<template>
  <!-- 页面骨架照原型 `devices.html:14-23`：`header.af-head` 是一整幅页头，
       内容在它下面的 `.af-body` 里。上一版是自造的 `.af-page` +
       `.af-page__head` —— 那一族规则在 v0.8.1 子任务 2 已随类名删除，于是这一页
       变成一张没有页头几何的裸页面。页头用共享的 `PageHead`（与另外 3 页同一
       写法），主标题/副标题照原型是 `.af-head__title` / `.af-head__sub`。
       根容器给 `.af-body`：它才是原型里承载滚动的那一层（`app.css:132`），
       少了它长列表会把整页顶出一条滚动条、页头跟着滚出视野。 -->
  <PageHead>
    <h1 class="af-head__title">设备</h1>
    <span class="af-head__sub">管理面 · 管的是机器的配置与健康</span>
    <template #actions>
      <button class="af-btn af-btn--sm" type="button" :disabled="loading" @click="refreshCurrent">
        刷新
      </button>
    </template>
  </PageHead>

  <div class="af-body">
    <div class="af-devpage__body af-stack">
    <!-- 这段说明条**不再占首屏**（v0.8.1 子任务 13）。原来它是页面最上面一块
         通栏 callout，三段散文、横铺 1130px——用户打开「设备」第一眼看到的是
         一篇讲心跳判定的说明书，而不是机器列表。

         其中三条事实仍然必须有去处，各自挪到了说它的地方：
           ·「每台设备一条 WebSocket 主动外联」→ 搬进下面的 `.af-dev-summary` 行。
           ·「45 秒心跳 / 重连中 / 离线」→ 搬进设备表 `.af-region__head` 的 note。
           ·「终端与文件是**会话内**的面板——这一页没有打开它们的入口」→ 这句是
             `DevicesPage.test.ts` 钉着的一句承诺，**必须在页面上可读**，不是注释。
             它现在作为 `.af-dev-summary` 里的一枚说明排在同一行。 -->

    <!-- 分段条：设备 / Agent。**用 tablist 语义**，因为它在两套内容之间切换
         而不是跳页；`aria-selected` 让屏幕阅读器知道当前在哪个分段。 -->
    <div class="af-row gap-2 af-seg" role="tablist" aria-label="设备页分段">
      <button
        class="af-btn af-btn--sm"
        :class="{ 'af-btn--primary': segment === 'devices' }"
        type="button"
        role="tab"
        :aria-selected="segment === 'devices'"
        @click="selectSegment('devices')"
      >
        设备
      </button>
      <button
        class="af-btn af-btn--sm"
        :class="{ 'af-btn--primary': segment === 'agents' }"
        type="button"
        role="tab"
        :aria-selected="segment === 'agents'"
        @click="selectSegment('agents')"
      >
        Agent 目录
      </button>
      <span class="af-spacer"></span>
      <span class="af-t-mute af-2xs">
        这两块看的是同一批机器：设备说它在不在线，目录说它上面装了什么
      </span>
    </div>

    <AgentCatalog
      v-if="segment === 'agents'"
      ref="catalog"
      @start-session="noSessionEntry"
    />

    <template v-else>
      <div class="af-row gap-4 af-dev-summary">
        <span class="af-t-mute"><span class="af-dot af-dev-summary__dot--ok"></span> 在线 {{ online.length }}</span>
        <span class="af-t-mute"><span class="af-dot af-dev-summary__dot--warn"></span> 重连中 {{ reconnecting.length }}</span>
        <span class="af-t-mute"><span class="af-dot af-dev-summary__dot--danger"></span> 离线 {{ offline.length }}</span>
        <span class="af-spacer"></span>
        <span class="af-t-mute af-t-num af-2xs">共 {{ devices.length }} 台</span>
      </div>

      <!-- 原来首屏那块三段式说明条压缩成的**一行事实**（v0.8.1 子任务 13）。
           三条内容一条不少，只是不再各占三行：
             ·「WebSocket 主动外联，不需要入站端口」——为什么 Hub 不用开端口；
             ·「终端与文件是**会话内**的面板——这一页没有打开它们的入口」——
               `DevicesPage.test.ts` 钉着的承诺，必须在页面上可读。
           测度收紧到 `--af-measure`，不再横铺 1130px。 -->
      <p class="af-dev-summary__note">
        每台设备一条 WebSocket 主动外联，Hub 不需要任何到设备的入站端口。
        终端与文件是<b>会话内</b>的面板——这一页没有打开它们的入口。
      </p>

      <div v-if="error" class="af-inline-error" role="alert">
        {{ error }}
        <button class="af-btn af-btn--sm" type="button" @click="load">重试</button>
      </div>

      <div v-if="removeError" class="af-inline-error" role="alert">
        {{ removeError }}
        <button class="af-btn af-btn--sm" type="button" @click="removeError = null">知道了</button>
      </div>

      <!-- 加载态是骨架，不是空态：两者混用会让用户以为设备没了（§4.8） -->
      <div v-else-if="loading" class="af-skel" aria-busy="true" aria-label="正在加载设备">
        <div v-for="i in 4" :key="i" class="af-skeleton af-skeleton--row"></div>
      </div>

      <!-- 空态 v0.8.1 子任务 13：**不再用居中的大虚线框**，改成一块有头有体的
           面板。理由：0 台设备时原空态是「242px 高的居中块 + 一句 48ch 宽的
           说明 + 内容只占左边一条」，看起来像加载失败；而这里其实有两件具体
           事要做（跑 afcli、下 afcli），所以它该长得像「待办清单」而不是
           「错误页」。`compact` 不适合这里——compact 是横排一行，装不下
           平台下拉框 + 下载按钮。 -->
      <section v-else-if="devices.length === 0" class="af-panel af-dev-empty" aria-label="还没有设备接入">
        <div class="af-panel__head">
          <h2 class="af-panel__title">还没有设备接入</h2>
          <span class="af-spacer"></span>
          <span class="af-region__note">首次启动会自动注册并生成密钥对，Hub 只存指纹</span>
        </div>
        <div class="af-panel__body af-dev-empty__body">
          <p>
            在设备上跑一条命令就会出现在这里：<code class="af-mono">afcli run &lt;hub-url&gt;</code>。
          </p>
          <!-- 上面那句「在设备上跑 afcli run」有个前提：那台设备上得**先有**
               afcli。离线/内网环境里够不到 GitHub，那段话就成了一句空话——
               所以这里给它一条实际的路。

               平台是**下拉框**而不是「按浏览器自动决定」：用户常常要给另一台
               架构不同的机器准备 afcli（Windows 上给 Linux 服务器备一份、x86
               Mac 上装 Apple Silicon 版），只按浏览器推断时没有任何入口能改。
               选项来自 `DOWNLOAD_PLATFORM_OPTIONS`，与后端闭集一一对应。 -->
          <div class="af-row gap-2 af-cli-dl">
            <label class="af-field__label" for="af-cli-platform">设备平台</label>
            <select
              id="af-cli-platform"
              v-model="downloadPlatformKey"
              class="af-select af-cli-dl__select"
            >
              <!-- 空值是「猜不到平台」时的落点。它必须是一个**真的选项**，
                   否则 `<select>` 会把第一项（Windows x64）当默认值显示出来，
                   用户看到的是一个自己没选过的平台。 -->
              <option value="">请选择平台</option>
              <option
                v-for="opt in DOWNLOAD_PLATFORM_OPTIONS"
                :key="`${opt.os}/${opt.arch}`"
                :value="`${opt.os}/${opt.arch}`"
              >
                {{ opt.label }}
              </option>
            </select>
            <button
              class="af-btn af-btn--sm"
              type="button"
              :disabled="downloadState === 'busy' || selectedPlatform === null"
              :title="selectedPlatform === null ? '请先选择要下载的平台' : undefined"
              @click="downloadCli"
            >
              {{ downloadState === 'busy' ? '正在准备下载…' : '下载 afcli' }}
            </button>
          </div>
          <!-- 失败一律**行内红字**，不弹 alert：alert 会阻断整页、且只能显示一句
               没法指到具体控件的话；这里紧挨着下载按钮，用户看得见自己在哪一步失败的。 -->
          <p v-if="downloadError" class="af-field__error" role="alert">{{ downloadError }}</p>
          <p v-else-if="selectedPlatform === null" class="af-field__hint">请选择平台</p>
        </div>
      </section>

      <template v-else>
        <!-- 原型 `prototype/devices.html:36-107`：设备表是一个 `.af-region`，
             标题写「N 台设备」，note 写「在线 X · 离线 Y」。此前这里是表格上方
             一行裸摘要，与原型的分区结构不一致——原型把计数放进**分区标题**，
             让「这一块是什么」和「里面有几台」在同一行说完。 |
             注意：上面那行 `.af-dev-summary` 保留，因为既有测试断言「在线 1」
             这类文案必须出现在页面上（`DevicesPage.test.ts`）；两者同源同值。 -->
        <section class="af-region af-devpage__table" aria-label="设备列表">
          <div class="af-region__head">
            <h2 class="af-region__title">{{ devices.length }} 台设备</h2>
            <span class="af-region__note">
              在线 {{ online.length }} · 重连中 {{ reconnecting.length }} · 离线 {{ offline.length }}
            </span>
          </div>
          <div class="af-region__body af-region__body--flush">
            <DeviceTable
              :devices="devices"
              :on-inspect="inspect"
              :on-create="createSession"
              :on-remove="remove"
            />
          </div>
        </section>

        <!-- 原型 `:109-221`：每台设备一块详情 region，内含四组
             （绑定关系 / 磁盘 / 失败归因 / 能不能删）。这里把内联详情对齐成
             「一个 region + 四组」的结构；数据齐的照旧显示，缺数据的组写明
             缺什么——留空会被当成「这台机器没有」，而事实是「这一版没读」。 -->
        <section
          v-if="inspected"
          class="af-region af-inspect af-devpage__detail"
          aria-label="设备详情"
        >
          <div class="af-region__head">
            <h2 class="af-region__title">{{ inspected.name }}</h2>
            <span class="af-badge">{{ inspected.platform || '平台未上报' }}</span>
            <span v-if="inspected.gpu" class="af-badge af-badge--ok">GPU</span>
            <span class="af-spacer"></span>
            <span class="af-region__note">心跳 {{ relativeTime(inspected.last_seen) }}</span>
            <button class="af-btn af-btn--sm af-btn--ghost" type="button" @click="inspected = null">
              收起
            </button>
          </div>
          <div class="af-region__body">
            <div class="af-devpage__grid">
              <!-- ① 机器本身（原有内容，一条不少） -->
              <div>
                <h3 class="af-devpage__group">机器</h3>
                <div class="af-kv">
                  <span class="af-kv__k">node_id</span>
                  <span class="af-kv__v af-fp af-selectable" title="设备身份锚：与列表里那一行显示的是同一个值">{{ inspected.id }}</span>
                  <span class="af-kv__k">密钥指纹</span>
                  <span class="af-kv__v af-fp" title="Hub 打码后的凭证指纹（前 3 + *** + 后 3）：凭证问题看它，身份看 node_id">{{ inspected.key_fingerprint }}</span>
                  <span class="af-kv__k">afcli</span>
                  <span class="af-kv__v">{{ inspected.node_version || '未上报' }}</span>
                  <span class="af-kv__k">资源</span>
                  <span class="af-kv__v">
                    CPU {{ percent(inspected.cpu) }} · 内存 {{ percent(inspected.mem) }}
                    <span v-if="inspected.cpu === null" class="af-t-mute">（未上报，不是 0%）</span>
                  </span>
                  <span class="af-kv__k">凭证</span>
                  <span class="af-kv__v af-t-mute">
                    设备私钥只在设备上生成、永不上传；Hub 存的只有上面那串指纹。
                  </span>
                </div>
              </div>

              <!-- ② 绑定关系（原型 `:121`）。「绑了哪些项目 + 各自的固定目录」
                   要读 `/api/devices/:id/detail`，列表接口没有这个字段。 -->
              <div>
                <h3 class="af-devpage__group">绑定关系</h3>
                <p class="af-devpage__p af-t-mute">
                  绑了哪些项目、各自钉在这台机器的哪个目录，要读设备详情接口。
                  这一块还没接——列表里的设备行只说机器在不在线。
                </p>
              </div>

              <!-- ③ 磁盘（原型 `:132`）。`DeviceSummary` 没有磁盘字段。 -->
              <div>
                <h3 class="af-devpage__group">磁盘</h3>
                <p class="af-devpage__p af-t-mute">
                  配额、已用、余量要靠设备心跳上报磁盘；这一版还没有这个字段，
                  所以这里不画一个没有来源的进度条。
                </p>
              </div>

              <!-- ④ 失败归因（原型 `:141`）。要按设备聚合已结束的执行。 -->
              <div>
                <h3 class="af-devpage__group">失败归因</h3>
                <p class="af-devpage__p af-t-mute">
                  失败集中在哪一类，要按设备聚合已结束的执行，归设备详情接口。
                  现在能看到的只有左侧这些心跳事实。
                </p>
              </div>
            </div>
          </div>
        </section>
      </template>
    </template>
    </div>
  </div>
</template>

<style scoped>
/* 原型 `devices.html:24`：`.af-body` 里那一层同时带 `padding:20px 20px 64px`
   和 `af-stack`（`display:flex; flex-direction:column; gap:16px`）。**两样都要**：
   只给 padding 的话，说明条与下面的分段条之间没有任何间距——原型不是那个样子，
   横向留白对、纵向挤在一起。padding 写到样式表里（原型是内联，实现侧不写内联），
   `af-stack` 直接复用原型已移植的类（`prototype-shell.css:506`）。 */
.af-devpage__body {
  padding: var(--p-s6) var(--p-s6) var(--p-s8);
}
.af-seg {
  margin-bottom: var(--af-space-5);
  align-items: center;
  gap: var(--af-space-2);
}
.af-dev-summary {
  margin-bottom: var(--af-space-5);
  font-size: var(--af-text-sm);
}
/* 摘要带上的三个状态点。**颜色取自语义令牌，不写在模板的内联样式里**：
   内联 `style="color: var(--af-ok)"` 与 token 分叉时没有测试会红，
   而主题（四套）切换时它跟着 token 走才是对的。
   `.af-dot` 底色是 `currentColor`（`components.css:279`），所以这里改的是
   **文字色**——与原来那三条内联样式同一个机制，只是挪进了样式表。 */
.af-dev-summary__dot--ok { color: var(--af-ok); }
.af-dev-summary__dot--warn { color: var(--af-warn); }
.af-dev-summary__dot--danger { color: var(--af-danger); }
/* 下载行：下拉框按内容宽度、按钮按内容宽度，别让 Select 的 `width: 100%`
   把按钮挤到第二行。这条只作用于本页的这个控件，不动全局 `.af-select`。 */
.af-cli-dl {
  align-items: center;
  flex-wrap: wrap;
}
.af-cli-dl__select {
  width: auto;
  min-width: 160px;
}
/* 设备表挂进 `.af-region` 后与下面的详情块留一点距离 */
.af-devpage__table {
  margin-bottom: var(--af-space-5);
}
/* 详情里的四组：窄屏一列，宽屏两列。
   **不用 `grid-template-columns: repeat(4, 1fr)`**：四组里三组现在是说明文字，
   四列会把每列挤到几十字宽、每行只放得下三四个字。 */
.af-devpage__grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: var(--af-space-4) var(--af-space-6);
}
/* 组标题：小号 + 弱化，与 `.af-region__title` 拉开层级（那是分区级，
   这是分区内的组级）。原型用的是大写 + 字距，这里沿用 token 里的字距变量。 */
.af-devpage__group {
  margin: 0 0 var(--af-space-2);
  font-size: var(--af-text-xs);
  font-weight: var(--af-weight-semibold);
  color: var(--af-fg-tertiary);
  text-transform: uppercase;
  letter-spacing: var(--af-tracking-label);
}
.af-devpage__p {
  margin: 0;
  font-size: var(--af-text-sm);
  line-height: var(--af-leading-base);
}
.af-inspect {
  margin-top: var(--af-space-5);
  padding: var(--af-space-4) var(--af-space-5);
  background: var(--af-surface-1);
  border: 1px solid var(--af-border-default);
  border-radius: var(--af-radius-md);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
/* `.af-kv` / `.af-kv__k` / `.af-kv__v` 已移到 `styles/prototype-shell.css` §8b：
   这四个类被 5 个页面用，而 scoped 块不外泄 —— 留在本文件里时其余 4 个页面
   拿到的是没有两列网格的裸 span。同一条规则只有一个家。 */
</style>
