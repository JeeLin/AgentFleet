//! terminal connector：真 PTY，子任务 8。
//!
//! **零入站端口**（约束 1）是这里的核心承诺：PTY 不是网络监听器，Hub 不
//! 需要能主动连进设备，所以会话内的终端面板不要求设备装 openssh-server。
//! 设备装一次 `afcli`、跑一条命令，终端就能用。
//!
//! **node 只搬字节，不做终端仿真**：ANSI 序列由页面的 xterm.js 解析
//! （`protocol.md` §4「`channel_data` 的 payload 本层只透传」）。这里如果
//! 开始替页面解释转义序列，两份实现必然对同一个 `\e[?25l` 给出不同画面。
//!
//! ## 载荷
//!
//! 下行（Hub → node，`channel_data`）：
//! - `{"bytes": "..."}` —— 用户敲键的字节
//! - `{"resize": {"cols": N, "rows": N}}` —— 窗口尺寸变化
//!
//! **resize 复用 `channel_data` 而不新增消息类型**（约束 14）：终端的控制
//! 命令与数据同源，都是「对这条通道说点什么」，拆出一个新 `t` 只为三种
//! 载荷换来一个永远只有一种用法的类型。
//!
//! 上行（node → Hub，`channel_data`）：`{"bytes": "..."}`。

use std::collections::VecDeque;

// 「读端」两平台类型不同：Unix 是 fd，Windows 是句柄。读线程只认
// `pty::ReadEnd` 这个别名，正文里不出现 `cfg`。
use crate::pty::{read_handle, OwnedReadEnd, ReadEnd};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{mpsc, Arc};

use serde::Deserialize;

use crate::connector::{Connector, ConnectorError, OpenReq, OutFrame};
use crate::pty::{default_shell, Pty, Size};

/// `open` 的参数。**全部可选**：页面给什么就用什么，缺的那几项落到
/// 一个「能用的默认值」上——开终端面板不是配置作业，让它因为少传一个
/// `rows` 就失败才是真的不好用。
#[derive(Debug, Default, Deserialize)]
struct TerminalParams {
    shell: Option<String>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    env: Vec<(String, String)>,
}

/// 下行帧。**两个字段各自可选，然后由本文件判定合法的组合**。
///
/// 不用 serde 的 enum 表示：默认的 externally tagged 形状是
/// `{"bytes": {"bytes": "..."}}`（struct variant 要再套一层），而
/// `untagged` 又会把「两个都没给」和「两个都给」都报成同一句
/// `data did not match any variant` —— 那句信息对用户毫无用处。
#[derive(Debug, Default, Deserialize)]
struct InFrameRaw {
    bytes: Option<String>,
    resize: Option<ResizeFrame>,
}

#[derive(Debug, Deserialize)]
struct ResizeFrame {
    cols: u16,
    rows: u16,
}

impl InFrameRaw {
    /// 恰好一种，且说得清是哪一种不对。
    fn into_action(self) -> Result<InFrame, ConnectorError> {
        let bad = |detail: String| ConnectorError::Invalid { detail };
        match (self.bytes, self.resize) {
            (Some(bytes), None) => Ok(InFrame::Bytes { bytes }),
            (None, Some(r)) => Ok(InFrame::Resize {
                cols: r.cols,
                rows: r.rows,
            }),
            (None, None) => Err(bad(
                "terminal data must carry either \"bytes\" or \"resize\"".to_string(),
            )),
            (Some(_), Some(_)) => Err(bad(
                "terminal data carries both \"bytes\" and \"resize\"".to_string()
            )),
        }
    }
}

/// 校验过的那一种下行帧。
enum InFrame {
    Bytes { bytes: String },
    Resize { cols: u16, rows: u16 },
}

/// 终端的默认尺寸。80×24 是所有终端的出厂默认，也是页面上「还没量到
/// 尺寸」时唯一站得住的值。
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// **下限 1×1**：0 会被 `TIOCSWINSZ` 收下，而内核把它当「未设置」，
/// 于是 `stty size` 报 `0 0`——那是内核的语义，不是我们该传下去的值。
const MIN_DIM: u16 = 1;

/// 读缓冲。**不是「攒够再发」**：PTY 是字节流，提示符必须在用户还没敲
/// 任何东西时就出现在页面上，等到攒满才发等于终端开局黑屏一秒。
const READ_CHUNK: usize = 16 * 1024;

/// 读线程与 `poll` 之间的交接缓冲上限。
///
/// 读线程**不能**无界地往里塞：PTY 的产出速率（`yes`、大日志）可以远超
/// 信用额度，而通道层的 backlog 有限。真堵死之后读线程得停读，否则
/// 一条没人读的通道能吃掉设备全部内存（`protocol.md` §6 的写侧约束）。
const READ_QUEUE_BYTES: usize = 1024 * 1024;

/// 队列字节数的原子计数器。**读线程与 `poll` 分处两个线程**，所以队列
/// 本身在 `Receiver` 里，长度必须靠共享原子量来判——`Receiver::len()`
/// 只给个数不给字节数。
type QueueBytes = Arc<std::sync::atomic::AtomicUsize>;

/// 终端通道的资源：一个真 PTY + 一个专职读它的线程。
///
/// **为什么必须是一个线程，而不是在 `poll` 里直接读**：PTY 的 `read` 在
/// 没有数据时阻塞。`poll` 是被通道层在**持有全局通道表锁**的情况下调用的，
/// 一次阻塞读就是一次全局停顿——同一个设备上的 files / exec 通道全被冻住，
/// 而规则 8 要求心跳与通道是两条独立路径。
///
/// 线程唯一的职责是「读 byte → 交出去」，不碰连接、不碰信用、不看协议。
/// 它比 `select!` 少一个可被取消的 future，也就少一类「取消发生在
/// 半次读中间」的边界情况。
#[derive(Debug)]
pub struct Terminal {
    pty: Option<Pty>,
    size: Size,
    /// 待推给 Hub 的输出字节。`poll` 只在这里取，不做 IO。
    pending: VecDeque<Vec<u8>>,
    /// 读线程交出来的队列。**本类型不保留发送端**：
    /// 只要还握着一份 `tx`，`try_recv` 就永远看不到 `Disconnected`，
    /// 于是「子进程已退出」这个事实永远传不上来，终端面板停在一块
    /// 不再动、也不报错的黑板上。
    rx: Option<Receiver<Vec<u8>>>,
    /// 「关掉面板」信号。读线程在两次读之间查它。
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    queued: Option<QueueBytes>,
    /// 队列里那条 `None`：**子进程已经退出，输出到此为止**。
    /// 这是「读完了」与「读失败了」的区别——前者要告诉用户命令跑完了，
    /// 后者才是故障（`pty.rs` 把 `EIO` 归到前者正是为此）。
    done: bool,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            pty: None,
            size: Size {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
            },
            pending: VecDeque::new(),
            rx: None,
            stop: None,
            queued: None,
            done: false,
        }
    }

    /// 已开着吗（`pty` 有值 = 开过且没关）。
    ///
    /// **生产侧没有调用点**：关通道走 [`Self::close`]，它自己就是幂等的
    /// （`pty.take()` 之后 `close` 什么也不做）。留着这个访问器是因为
    /// 测试要断言「开过 / 没开过 / 关过了」三种状态，而 `self.pty.is_some()`
    /// 在测试模块外不可见。
    pub fn is_open(&self) -> bool {
        self.pty.is_some()
    }

    fn pty(&self) -> Result<&Pty, ConnectorError> {
        self.pty.as_ref().ok_or_else(|| ConnectorError::Invalid {
            detail: "terminal is not open".to_string(),
        })
    }

    fn detach(&mut self) {
        self.pty = None;
        self.rx = None;
        self.stop = None;
        self.queued = None;
        self.pending.clear();
        self.done = false;
    }
}

/// 0 与缺失同义。
fn non_zero(v: Option<u16>) -> Option<u16> {
    v.filter(|n| *n > 0)
}

/// 尺寸校验。**下限而不是上限**：页面上一个被拖到极窄的窗口真的会给很小的
/// `cols`，直接拒绝会让「拖窄了终端就打不开」这种最正常的操作失败。
fn checked(cols: u16, rows: u16) -> Result<Size, ConnectorError> {
    if cols < MIN_DIM || rows < MIN_DIM {
        return Err(ConnectorError::Invalid {
            detail: format!(
                "terminal size must be at least {MIN_DIM}x{MIN_DIM}, got {cols}x{rows}"
            ),
        });
    }
    Ok(Size { cols, rows })
}

impl Connector for Terminal {
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        let p: TerminalParams = req
            .parse("terminal")
            .map_err(|e| ConnectorError::Invalid { detail: e })?;
        // 0 视作「没给」：页面刚挂载、还没量到尺寸时会发 0，
        // 而 0 对内核是「未设置」，`stty size` 会报 `0 0`。回落到默认比
        // 报错更接近用户要的——他要的是一个能看见提示符的终端。
        //
        // `?` 的那条 Err 因此**结构上不可达**：`non_zero` 只放行 ≥1，而
        // `MIN_DIM` 也是 1，两个入口都不会让 `checked` 失败。下面的 `?`
        // 留着是因为「以后若把 MIN_DIM 调大，这里要立刻跟着拒绝」。
        // coverage: excluded — 理由见上：`checked` 在 `open` 里恒成功。
        let size = checked(
            non_zero(p.cols).unwrap_or(DEFAULT_COLS),
            non_zero(p.rows).unwrap_or(DEFAULT_ROWS),
        )?;

        // shell 优先用页面给的，其次设备自己的默认。空串当「没给」——
        // 页面上一个空输入框提交上来是空串，不是「请执行空路径」。
        let shell = p
            .shell
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_shell);

        // cwd 跟着会话走（约束 3：terminal 绑会话工作区 `(node_id, path)`）。
        // 没有工作区就用 worker 自己的 cwd，不猜。
        let cwd = p.cwd.filter(|c| !c.is_empty());
        let cwd_ref = cwd.as_ref().map(std::path::Path::new);

        self.close();
        let pty = Pty::spawn_in(&shell, &[], size, cwd_ref, &p.env).map_err(|e| {
            ConnectorError::Failed {
                op: "start terminal".to_string(),
                detail: e.to_string(),
            }
        })?;
        self.pty = Some(pty);
        self.size = size;
        self.start_reader()?;
        // 首帧不回：shell 的提示符由 `poll` 泵出去。回一个空帧列表，
        // 让通道层照常发 `channel_ready` —— 页面上「面板已就绪」与
        // 「提示符已显示」是两个状态，混成一帧会让后者早于前者成立。
        Ok(Vec::new())
    }

    fn on_data(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        // 先确认开着：`data` 反序列化失败时错误信息会指向「帧格式」，
        // 而真实原因是这条通道早关了——后者才是用户要看到的那句。
        self.pty()?;
        let frame: InFrame = serde_json::from_value::<InFrameRaw>(data.clone())
            .map_err(|e| ConnectorError::Invalid {
                detail: format!("bad terminal data: {e}"),
            })?
            .into_action()?;
        match frame {
            InFrame::Bytes { bytes } => {
                self.pty()?
                    .write(bytes.as_bytes())
                    .map_err(|e| ConnectorError::Failed {
                        op: "write to terminal".to_string(),
                        detail: e.to_string(),
                    })?;
                Ok(Vec::new())
            }
            InFrame::Resize { cols, rows } => {
                let size = checked(cols, rows)?;
                // coverage: excluded — 上面 `self.pty()?` 已经确认开着，同一
                // 函数里 `self.pty` 不可能从 Some 变成 None（改它的 `close` /
                // `detach` 都是别的入口），所以这个 None 臂结构上不可达。
                let pty = self.pty.as_mut().ok_or_else(|| ConnectorError::Invalid {
                    detail: "terminal is not open".to_string(),
                })?;
                pty.resize(size).map_err(|e| ConnectorError::Failed {
                    op: "resize terminal".to_string(),
                    detail: e.to_string(),
                })?;
                self.size = size;
                Ok(Vec::new())
            }
        }
    }

    /// 把攒着的输出抽出来。**不做任何 IO**——见 [`Terminal`] 的线程理由。
    fn poll(&mut self) -> Result<Vec<OutFrame>, ConnectorError> {
        self.drain();
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        // **一帧一条**：PTY 的输出是连续的字节流，而每个 credit 只值一帧。
        // 一次 `poll` 吐 N 条就吃掉 N 个 credit，而同一 tick 里别的通道
        // 可能正等着这个额度——合并成一条是拿别人的额度填自己的队列。
        let mut out = Vec::new();
        while let Some(chunk) = self.pending.pop_front() {
            out.push(OutFrame::bytes(chunk)?);
        }
        Ok(out)
    }

    fn close(&mut self) {
        // 顺序要紧：先喊停，再关 master。
        //
        // 读线程此刻多半正**阻塞在 `read` 里**（PTY 空闲时就是这样），
        // 所以光关 master 叫不醒它——它手里那份 dup 仍然开着内核的
        // master，`SIGHUP` 也因此发不出去，shell 会带着这个读线程
        // 一直活到设备重启。停标志让它在下一个 poll 周期自己退出。
        if let Some(stop) = self.stop.as_ref() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(mut pty) = self.pty.take() {
            // 关 master 让前台进程组收到 `SIGHUP`，也就是「关掉终端面板 =
            // 关掉那个 shell」，而不是让它在设备上继续跑着没人管。
            pty.close();
        }
        self.detach();
        // 关掉的面板**永远不会再有输出了**，所以它就是「读完了」。
        // 不置位的话调用方只能靠「`pty` 是 None」去猜，于是每一个
        // 问「这个终端结束了吗」的地方都要自己再写一遍这个判断。
        self.done = true;
    }

    /// **shell 退出、且攒着的输出都排空了**才算跑完。
    ///
    /// 两个条件缺一不可：shell 退了但队列里还有字节时，那点字节仍然要发给
    /// 页面——此时判「跑完」会让终端的最后几行永远发不出去。
    fn finished(&self) -> bool {
        self.done && self.pending.is_empty()
    }
}

impl Terminal {
    /// 起读线程。**PTY 建不起来就不该有线程**，所以它在 `open` 末尾调。
    // coverage: excluded — 下面 `self.pty` 的 None 臂：`open` 在设好
    // `self.pty = Some(pty)` 之后才调这里，同一个 `Terminal` 实例上没有
    // 别的路径能在调用前把它摘掉（`detach` 只被 `close` 调）。
    fn start_reader(&mut self) -> Result<(), ConnectorError> {
        let Some(pty) = self.pty.as_ref() else {
            return Err(ConnectorError::internal("terminal is not open"));
        };
        // coverage: excluded — `dup_master` 走 `fcntl(F_DUPFD)`，失败需 fd
        // 表耗尽（约 1M 个打开的描述符），测试环境无可控构造手段。
        let dup = pty.dup_master().map_err(|e| ConnectorError::Failed {
            op: "attach to terminal".to_string(),
            detail: e.to_string(),
        })?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_SLOTS);
        let queued: QueueBytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&queued);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // `tx` 只交给读线程：本类型不保留它（见字段注释）。
        // coverage: excluded — `spawn` 失败需线程数达上限或内存耗尽，
        // 测试环境无可控构造手段。
        std::thread::Builder::new()
            .name("afcli-pty".to_string())
            .spawn(move || read_loop(raw_handle_of(&dup), tx, counter, flag))
            .map_err(|e| {
                ConnectorError::internal(format!("cannot start the terminal reader: {e}"))
            })?;
        self.rx = Some(rx);
        self.stop = Some(stop);
        self.queued = Some(queued);
        self.done = false;
        Ok(())
    }

    /// 把读线程队列里的东西挪进 `pending`，能挪多少挪多少。
    fn drain(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(rx) = self.rx.as_ref() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    if let Some(q) = self.queued.as_ref() {
                        q.fetch_sub(chunk.len(), Relaxed);
                    }
                    self.pending.push_back(chunk.to_vec());
                }
                // 子进程退了：发送端析构就是「输出到此为止」的信号，而不是
                // 一次失败。用户该看到的是「命令跑完了」。
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    return;
                }
                // 队列空了。这**不代表**输出结束——下一批可能正在路上，
                // 所以 `done` 不能在这里置位，否则会在命令还没跑完时
                // 就把终端判死。
                Err(TryRecvError::Empty) => return,
            }
        }
    }
}

/// 队列槽位数。**按 64 KiB 一槽算**：与 [`READ_CHUNK`] 配套，读线程每次
/// 正好填满一槽，槽数也就是队列的字节上限（`READ_QUEUE_BYTES`）。
const QUEUE_SLOTS: usize = READ_QUEUE_BYTES / READ_CHUNK;

/// 等到队列账上的字节数掉回 `budget` 以下。
///
/// **抽成函数是为了能测**：写在 `read_loop` 里就只能靠「真把 1 MiB 灌满
/// 再停住消费端」来撞，那是个时序测试，慢且偶发；这里直接喂一个已经
/// 越线的账就能验「它会等、等到了会走」。
fn wait_for_budget(queued: &QueueBytes, budget: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    while queued.load(Relaxed) >= budget {
        std::thread::yield_now();
    }
}

/// 读线程。**只做「读 byte → 交出去」**：不碰连接、不碰信用、不看协议。
///
/// 阻塞在 `read` 是这个线程的全部工作——它在没有输出时该睡就睡，而那正是
/// 它存在的理由：谁在会话循环里替大家挡住这个阻塞，谁就得同时持有
/// 整个通道表。
fn read_loop(
    master: ReadEnd,
    tx: SyncSender<Vec<u8>>,
    queued: QueueBytes,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    // **只借，不接管所有权**：另一次 `close()` 会关掉那一侧，而一个也关它
    // 的 `File` / `OwnedHandle` 会造成同一内核对象两个所有者，进程在 IO
    // safety 检查上直接 abort。读这一侧只需要一次读，不必带所有权。
    let fd = master;
    let mut buf = [0u8; READ_CHUNK];
    loop {
        // **每次读之前先给「关掉面板」一个机会**。`read(2)` 在 PTY 空闲时
        // 阻塞，所以只靠关闭 fd 叫不醒这个线程（见 [`Terminal::close`]）。
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if !crate::pty::wait_readable(fd, READ_POLL_INTERVAL) {
            // 超时 = 这一轮没有数据。回到循环头部查停标志。
            continue;
        }
        // `Ok(0)` 与 `Err` 都是「这一侧到此为止」：PTY 在子进程退出后
        // 报的是 `EIO` 而不是 EOF（`pty.rs` 把两者都归到这一类）。
        let n = match read_handle(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(chunk) = buf.get(..n) else {
            // coverage: excluded — `read_handle` 最多读 `buf.len()` 字节，
            // 所以 `buf.get(..n)` 恒为 Some（`n <= buf.len()`）。留着手写
            // 是因为裸切片会被 workspace lint 拒掉。
            break;
        };
        // **先记账再发**：`sync_channel` 的槽位只管个数、不管字节数，
        // 而这里要卡的是字节数（一条 `yes` 一次能产出几十 MiB）。
        wait_for_budget(&queued, READ_QUEUE_BYTES);
        let len = chunk.len();
        queued.fetch_add(len, Relaxed);
        if tx.send(chunk.to_vec()).is_err() {
            // 通道关了：没人再消费。把刚才记的账退掉，否则这次连接的
            // 字节数会一直虚高，下一条通道一上来就在等一个不存在的额度。
            queued.fetch_sub(len, Relaxed);
            break;
        }
    }
    // 发送端析构 → `try_recv` 得到 `Disconnected` → `drain` 置 `done`。
    drop(tx);
}

/// 从「拥有所有权的读端」借出一个裸值给读线程。
///
/// 读线程只借不拿：`dup` 仍由 `start_reader` 的栈持有到 `spawn` 之后……
/// ——但 `spawn` 把它 move 进闭包了，所以闭包持有着它，`raw_handle_of`
/// 在闭包里调，借出来的裸值在闭包存活期间一直有效。
#[cfg(unix)]
fn raw_handle_of(end: &OwnedReadEnd) -> ReadEnd {
    use std::os::fd::AsRawFd;
    end.as_raw_fd()
}

#[cfg(windows)]
fn raw_handle_of(end: &OwnedReadEnd) -> ReadEnd {
    use std::os::windows::io::AsRawHandle;
    end.as_raw_handle()
}

/// 读线程的等待粒度。**它同时是「关掉面板」到线程真正退出之间的上界**：
/// 太大则关掉的终端还占着 master 与一个线程几秒，太小则空转烧电。
const READ_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// 「能用的默认值」必须真的存在：注册表按名造 connector 时走的就是
/// `Default`，而一个只能 `new()` 的类型没法进那张表。
impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

/// 真 PTY 的行为（提示符会自己出来、命令的输出会自己回来、resize 落到
/// 内核上、子进程退了会被认出来）。**判据是真读内核的答案**，不是
/// 「我们自己记住了一个变量」。
///
/// ## 为什么整块 `cfg(all(test, unix))`
///
/// 这个模块里的每一条用例都通过 `Pty::spawn("/bin/sh")` 起一个**真的
/// `/bin/sh`**，命令用 `echo` / `stty`，`cwd` 写死 `/tmp`，断言的是
/// 「字符被 PTY 回显」「`stty size` 读回新尺寸」这类**POSIX 终端语义**。
/// Windows 上这些前提一条都不成立：没有 `/bin/sh`、没有 `/tmp`、ConPTY 的
/// 回显时序也不一样。不挡的话，`test-desktop` 的 windows runner 会在
/// `spawn_in` 的 `ERROR_FILE_NOT_FOUND` 上直接 panic——那不是「发现了一个
/// bug」，而是一条本来就跑不了的用例把 CI 染红，把真正该看到的红盖掉。
///
/// **代价要说清**：Windows 的 PTY 行为因此在 CI 上没有行为验证，只有
/// `cargo check --target x86_64-pc-windows-msvc` 的编译验证。这是本里程碑
/// 已知的验证缺口，记在 `docs/DEVELOPMENT.md` 的「已知行为差异」一节和
/// 里程碑报告的「遗留风险」里，不假装它不存在。
// `clippy.toml` 的 `allow-expect-in-tests` 只覆盖 `#[test]` 函数体与
// `#[cfg(test)] mod`；这里多了 `unix` 条件，模块内**非 `#[test]` 的 helper
// 函数**（如 `run_to_completion`）不再落在它的边界里，需在这里补齐。
// 作用范围只到这个测试模块，生产代码仍由 workspace lint 表 deny。
#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::connector::names;

    fn req(params: serde_json::Value) -> OpenReq {
        OpenReq::new("/tmp", params)
    }

    /// 真带一个 NUL 字节的 shell 名。写 `\0` 转义会被 serde 当成两个字符，
    /// 那测不到「C 字符串塞不进 NUL」这件事。
    fn bad_nul_name() -> String {
        let mut s = String::from("bad");
        s.push('\0');
        s.push_str("shell");
        s
    }

    /// 起一个 shell，往里敲一条命令，等到它退出，吐出全部输出。
    ///
    /// **走的是和用户完全一样的路**（敲键 → `on_data` → 输出 → `poll`），
    /// 所以这个测试验证的是接线的整体，而不是单个零件。
    /// 跑一条命令直到 shell 退出，返回 `(收到的文本, 是否退出)`。
    ///
    /// **60 s 上限**：这是真 fork 真 shell，而全仓并行跑测试时 CPU 被抢，
    /// 10 s 在 `-j 2` 下偶发不够——判据是真的（一个字都没生完），
    /// 但它会随机假红，而随机假红的测试比没有测试更坏：它训练人忽略
    /// 红灯。上界比单次运行的耗时大一截就够，成本只是挂到上限的耗时。
    fn run_to_completion(cmd: &str) -> (String, bool) {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        t.on_data(&serde_json::json!({ "bytes": format!("{cmd}\nexit\n") }))
            .expect("type the command");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut text = String::new();
        loop {
            for f in t.poll().expect("poll") {
                if let Some(s) = f.payload.get("bytes").and_then(|b| b.as_str()) {
                    text.push_str(s);
                }
            }
            if t.finished() {
                return (text, true);
            }
            if std::time::Instant::now() > deadline {
                return (text, t.finished());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// **关掉之后** `pty`/`rx` 都被摘掉，于是「终端没开」那三条路
    /// （`start_reader` 的 :327、resize 的 :264）各自说得清原因。
    ///
    /// 这三条路的错误必须是「没开」而不是「帧格式不对」：真实原因常常是
    /// 这条通道早关了，而用户要看到的是后者。
    #[test]
    fn a_closed_terminal_refuses_work_by_saying_it_is_not_open() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        assert!(t.is_open(), "开着");
        t.close();
        assert!(!t.is_open(), "关掉之后不该还开着");

        // resize 走 `self.pty()` 那条「没开」的臂
        let err = t
            .on_data(&serde_json::json!({"resize":{"cols":80,"rows":24}}))
            .expect_err("关掉的终端不能 resize");
        assert!(
            err.to_string().contains("not open"),
            "错误要指向「没开」而不是帧格式：{err}"
        );
        // 写字节同样
        let err = t
            .on_data(&serde_json::json!({"bytes":"ls\n"}))
            .expect_err("关掉的终端不能写");
        assert!(
            err.to_string().contains("not open"),
            "错误要指向「没开」而不是帧格式：{err}"
        );
        // 关掉的终端永远不会再有输出：`finished` 必须为真
        assert!(t.finished(), "关掉的面板不会再产出任何东西");
        assert!(t.poll().expect("poll").is_empty(), "关掉之后泵不出字节");
    }

    /// 尺寸下限：**拖窄窗口不该让终端打不开**。
    ///
    /// 判据是「1x1 也开得出来」而不是「某个具体尺寸被拒」——上限容易随
    /// 设计改动漂移，而「极窄也要能用」是那条不变量本身。
    #[test]
    fn a_very_small_size_is_still_allowed_but_zero_is_not() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({
            "shell": "/bin/sh", "cols": 1, "rows": 1
        })))
        .expect("1x1 必须开得出来");
        t.close();

        let t2 = Terminal::new();
        // 0 与「没给」同义 → 回落到默认尺寸，所以直接构造 `checked` 来验下限。
        let err = checked(0, 0).expect_err("0x0 必须被拒");
        assert!(
            err.to_string().contains("at least"),
            "错误要说清下限是多少：{err}"
        );
        let _ = t2;
    }

    /// 扫出文本里所有**恰好 8 位**的十进制串，按出现顺序返回。
    ///
    /// 「恰好」是重点：9 位以上的连续数字（比如回显出来的命令本身）会被
    /// 拆成 8 + 1 两个片段算进来，于是把一行误判成两个数。
    fn eight_digit_runs(text: &str) -> Vec<u32> {
        let b = text.as_bytes();
        let is_digit = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_digit());
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(window) = b.get(i..) {
            if window.len() < 8 {
                break;
            }
            let before_is_digit = i > 0 && is_digit(i - 1);
            let after_is_digit = is_digit(i + 8);
            let run: Option<u32> = b
                .get(i..i + 8)
                .filter(|w| w.iter().all(|c| c.is_ascii_digit()))
                .filter(|_| !before_is_digit && !after_is_digit)
                .map(|w| w.iter().fold(0u32, |a, c| a * 10 + u32::from(c - b'0')));
            match run {
                Some(v) => {
                    out.push(v);
                    i += 8;
                }
                None => i += 1,
            }
        }
        out
    }

    #[test]
    fn opening_it_starts_a_shell_and_reports_nothing_until_polled() {
        let mut t = Terminal::new();
        let frames = t.open(req(serde_json::json!({}))).expect("open");
        assert!(frames.is_empty(), "提示符走 poll，不走首帧");
        assert!(t.is_open());
        assert_eq!(
            t.size,
            Size {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS
            }
        );
    }

    /// **核心测试**：输出是**主动**推的，不等任何下行数据帧。
    ///
    /// 这正是那个只有 `on_data` 时看不见的缺陷：敲完 `echo` 就干等，
    /// 输出永远不来——终端看着像坏了。所以断言只要求「什么都没往下发，
    /// 却有字节回来了」，不要求 `on_data` 被调用过。
    ///
    /// 判据刻意**不写死提示符字符**：root 的 `sh` 打 `#`、普通用户打 `$`，
    /// 写死任何一个都会让这个测试在另一种容器里假红。
    #[test]
    fn the_prompt_shows_up_without_any_further_downlink() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut seen = String::new();
        while std::time::Instant::now() < deadline && seen.trim().is_empty() {
            for f in t.poll().expect("poll") {
                if let Some(s) = f.payload.get("bytes").and_then(|b| b.as_str()) {
                    seen.push_str(s);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!seen.trim().is_empty(), "一个字节都没主动推出来");
    }

    /// 敲进去的命令会被 shell 真的执行，输出会真的回来。
    #[test]
    fn echo_round_trips_through_a_real_shell() {
        let (text, done) = run_to_completion("echo agentfleet-pty-round-trip");
        assert!(done, "shell 没在 10s 内退出：{text}");
        assert!(
            text.contains("agentfleet-pty-round-trip"),
            "echo 的输出没回来：{text}"
        );
    }

    /// 子进程退出后 `finished` 为真，且**再 poll 也不产出**——
    /// 一个永动的空轮询会把 CPU 和电量都烧掉。
    #[test]
    fn a_finished_shell_stays_finished() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        t.on_data(&serde_json::json!({ "bytes": "exit\n" }))
            .expect("type exit");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !t.finished() && std::time::Instant::now() < deadline {
            t.poll().expect("poll");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(t.finished(), "退出了的 shell 没被认出来");
        assert!(t.poll().expect("poll after finish").is_empty());
        assert!(t.poll().expect("poll again").is_empty());
    }

    /// **大输出不丢字节**：产物比单次读缓冲大 100 多倍，正好把「攒够再发」
    /// 与「截断到协议上限」这两种做法逼成 bug。
    ///
    /// 判据不是「行数差不多」，而是**把收到的字节里所有 8 位数字串扫出来，
    /// 要求它们恰好是 `0..N-1` 且严格递增**。按行数计会漏掉一种失败：
    /// 首行被提示符粘住就不算独立的一行，看起来只少一行，其实是粘住。
    /// 扫数字串与 shell 的提示符/回显怎么交错无关。
    #[test]
    fn output_bigger_than_one_buffer_survives_intact() {
        const N: usize = 200_000; // READ_CHUNK（16 KiB）的 12 倍
        let (text, done) = run_to_completion(&format!(
            "i=0; while [ $i -lt {N} ]; do printf '%08d\\n' $i; i=$((i+1)); done; printf 'END-OF-MARKER\\n'"
        ));
        assert!(done, "生成 {N} 行没跑完");
        assert!(
            text.contains("END-OF-MARKER"),
            "命令没跑到最后一行，后段字节整个丢了"
        );

        let seq = eight_digit_runs(&text);
        assert_eq!(
            seq.len(),
            N,
            "收到 {} 个 8 位数字串（应为 {N}）；末尾原文={:?}",
            seq.len(),
            text.chars()
                .rev()
                .take(48)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        assert_eq!(seq.first(), Some(&0), "第一行不是 00000000");
        assert_eq!(
            seq.last(),
            Some(&((N - 1) as u32)),
            "最后一行不是 000199999"
        );
        for (i, v) in seq.iter().enumerate() {
            assert_eq!(*v as usize, i, "第 {i} 行对不上");
        }
    }

    #[test]
    fn the_registry_routes_the_terminal_name_here() {
        let mut c = crate::connector::create(names::TERMINAL).expect("registered");
        c.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open through the registry");
    }

    #[test]
    fn the_shell_a_page_names_wins_over_the_device_default() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        assert!(t.is_open());
    }

    /// 不存在的 shell 路径：`execvp` 失败后子进程 `_exit(127)`，父进程
    /// 看到的是**读干净了**而不是「起不来」。
    #[test]
    fn a_shell_that_is_not_there_reads_as_the_panel_ending_not_as_a_failure() {
        let mut t = Terminal::new();
        // `forkpty` 本身成功：路径存不存在是 exec 之后由内核判的。
        t.open(req(serde_json::json!({ "shell": "/nonexistent/shell" })))
            .expect("forkpty itself succeeds");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !t.finished() && std::time::Instant::now() < deadline {
            t.poll().expect("poll");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            t.finished(),
            "起不来的 shell 不会自己退出，终端会永远停在空面板上"
        );
    }

    #[test]
    fn a_shell_name_carrying_a_nul_is_refused_before_forking() {
        let mut t = Terminal::new();
        let err = t
            .open(req(serde_json::json!({ "shell": bad_nul_name() })))
            .expect_err("must refuse");
        assert!(err.to_string().contains("NUL"), "got {err}");
        assert!(!t.is_open(), "开不起来就不该算已打开");
    }

    #[test]
    fn the_reader_thread_stops_when_the_panel_is_closed() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        t.close();
        // 线程退出后 `drain` 会看到 Disconnected；这里只断言没有残留的
        // 待发数据——一个关不掉读线程的实现会在这里留下 backlog。
        assert!(t.poll().expect("poll after close").is_empty());
        assert!(t.finished(), "关掉的面板不该还在读");
    }

    /// 读线程在 stop 标志**已经置位**时立刻退出，不去读那一侧的 fd。
    ///
    /// 为什么单独测这一条：面板被关掉时 `close` 只置标志，真正的 `break`
    /// 发生在读线程下一轮循环的顶部。断言「线程退出了」还不足以说明它
    /// 走的是 stop 这条路——它也可能是读到了 EOF 才退的。
    ///
    /// **限 Unix**：这个用例要造一根「永远不会 EOF 的读端」，靠的是
    /// `pipe2` 之后**握着写端不关**。Windows 上的匿名管道同样有「写端没关
    /// 就不 EOF」的语义，但 `read_loop` 在那边收的是 `HANDLE` 而不是
    /// `RawFd`，且没有 `pipe2` 这种一步拿到非继承管道的调用——需要真
    /// ConPTY 才能验，只能在 CI 的 windows runner 上跑。
    #[cfg(unix)]
    #[test]
    fn the_reader_thread_returns_immediately_when_stop_is_already_set() {
        let mut fds = [-1 as std::os::fd::RawFd; 2];
        // **`pipe` + `fcntl(FD_CLOEXEC)`，不直调 `pipe2`**：`pipe2` 是 Linux
        // 扩展，macOS 上没有这个符号。这条用例的 `#[cfg(unix)]` 在 macOS 上
        // 为真，直调会让 macOS 的 `--all-targets` 编不过。
        // coverage: excluded — `pipe` 失败需 fd 表耗尽。
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let read_end =
            unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) };
        // 写端留着不关：这样这一侧永远等不到 EOF，读线程若不是被 stop
        // 唤醒的，就只能靠「stop 已置位」这一条路退出。
        let _write_end =
            unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fds[1]) };

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let queued: QueueBytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        read_loop(
            raw_handle_of(&read_end),
            tx,
            std::sync::Arc::clone(&queued),
            stop,
        );

        // 它退出了：发送端随之析构，接收端拿到 Disconnected。
        assert!(
            rx.recv().is_err(),
            "stop 已置位时读线程必须退出，而不是留在 read 上"
        );
    }

    /// 字节闸：账越过限额时**必须等**，等它掉回限额以下才放行。
    ///
    /// 这条闸失守的后果是「一条 `yes` 顶爆整台设备的内存」，所以它得
    /// 真的等，而不是「看一眼超了就当没超」。
    #[test]
    fn the_byte_budget_waits_until_the_queue_drops_below_the_limit() {
        let queued: QueueBytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let budget = 16usize;
        // 账已经越线：闸必须拦住。
        queued.store(budget * 2, std::sync::atomic::Ordering::Relaxed);

        let q = std::sync::Arc::clone(&queued);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            q.store(budget - 1, std::sync::atomic::Ordering::Relaxed);
        });

        let started = std::time::Instant::now();
        wait_for_budget(&queued, budget);
        let waited = started.elapsed();

        releaser.join().expect("放行线程");
        assert!(
            waited >= std::time::Duration::from_millis(40),
            "闸必须等账掉下来，实际只等了 {waited:?}"
        );
        assert!(
            queued.load(std::sync::atomic::Ordering::Relaxed) < budget,
            "放行时账必须在限额以下"
        );
    }

    #[test]
    fn an_empty_size_field_falls_back_to_the_default() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "cols": 0, "rows": 0 })))
            .expect("open");
        assert_eq!(
            t.size,
            Size {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS
            }
        );
    }

    #[test]
    fn a_resize_reaches_the_kernel_through_stty() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        t.on_data(&serde_json::json!({ "resize": { "cols": 100, "rows": 40 } }))
            .expect("resize");
        assert_eq!(
            t.size,
            Size {
                cols: 100,
                rows: 40
            }
        );
        assert_eq!(
            t.pty().expect("open").size(),
            Size {
                cols: 100,
                rows: 40
            },
            "内核那边也要是新的，否则 resize 只是我们自己改了个变量"
        );
    }

    #[test]
    fn a_zero_size_resize_is_refused_rather_than_handed_to_the_kernel() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        let err = t
            .on_data(&serde_json::json!({ "resize": { "cols": 0, "rows": 24 } }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("at least"), "got {err}");
    }

    #[test]
    fn data_on_a_closed_terminal_says_the_terminal_is_gone() {
        let mut t = Terminal::new();
        let err = t
            .on_data(&serde_json::json!({ "bytes": "ls\n" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not open"), "got {err}");
    }

    #[test]
    fn an_unrecognized_payload_names_the_connector_it_belongs_to() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({}))).expect("open");
        let err = t
            .on_data(&serde_json::json!({ "paste": "hi" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("terminal"), "got {err}");
    }

    /// 载荷解不开时错误要带上 connector 名：同一个 `params` 在不同
    /// connector 下含义完全不同，serde 自己的错误不含「这是给谁的」。
    #[test]
    fn a_payload_of_the_wrong_shape_names_the_terminal() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({}))).expect("open");
        // `bytes` 必须是字符串；给个数字解不出来。
        let err = t
            .on_data(&serde_json::json!({ "bytes": 42 }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("bad terminal data"), "got {err}");
    }

    /// 往一个**从端已关**的 PTY 写：内核回 `EIO`，这里必须变成一句
    /// `write to terminal` 而不是 panic 或静默吞掉。
    ///
    /// 触发方式是直接关掉 master 再写——`Pty::write` 的 `Closed` 臂在
    /// `pty.rs` 里已单独测过，这里测的是**上层把 `PtyError` 翻成人话**
    /// 这一段接线（`terminal.rs` 自己那一层）。
    ///
    /// 不用「让 shell 退出」来构造：shell 退出后 master 仍可写（内核
    /// 只是把读到的那一侧报成 EIO），写会**成功**——那是一条假绿。
    #[test]
    fn a_write_error_is_reported_as_a_write_failure_not_swallowed() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        // 直接拆掉 master：`on_data` 的 `self.pty()?` 仍通过（`pty` 是
        // `Some`，只是里面的 fd 没了），于是走到 `write` 的 `Closed` 臂。
        if let Some(pty) = t.pty.as_mut() {
            pty.close();
        }
        let err = t
            .on_data(&serde_json::json!({ "bytes": "ls\n" }))
            .expect_err("writing to a closed pty must fail");
        assert!(err.to_string().contains("write to terminal"), "got {err}");
        t.close();
    }

    #[test]
    fn closing_twice_is_allowed() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({}))).expect("open");
        t.close();
        t.close();
        assert!(!t.is_open());
        // 关完之后 poll 不该报错：断连与 close_channel 可能各来一次。
        assert!(t.poll().expect("poll after close").is_empty());
    }

    /// **两个都给**必须被拒，而且说的是「两个都给了」。
    ///
    /// 静默挑一个（比如总是信 `bytes`）会把一次拖窗口变成一次乱敲——
    /// 而用户看到的只是「终端里多出一串字符」，与根因相距最远的一种。
    #[test]
    fn a_frame_carrying_both_bytes_and_resize_is_refused_by_name() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({}))).expect("open");
        let err = t
            .on_data(&serde_json::json!({
                "bytes": "x",
                "resize": { "cols": 100, "rows": 40 }
            }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("both"), "got {err}");
    }

    /// `Default` 是注册表按名造 connector 时走的那条路（`create` → `new`）。
    /// 一个只能 `new()` 的类型进不了那张表，所以默认值必须真的给出
    /// 一个「没开但能开」的实例。
    #[test]
    fn the_default_terminal_is_closed_and_openable() {
        let mut t = Terminal::default();
        assert!(!t.is_open(), "默认实例不该凭空占一个 PTY");
        assert!(!t.finished(), "没开过的终端不是「跑完了」");
        assert!(t.poll().expect("poll").is_empty());
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        assert!(t.is_open());
    }

    /// `open` 的私有参数解不开：错误要带上 connector 名。
    ///
    /// 同一个 `params` 在不同 connector 下含义完全不同，而 serde 自己的
    /// 错误信息里没有「这是给谁的」——`rows` 给了个字符串时，页面要能
    /// 看出是 terminal 的参数不对，而不是「设备不支持终端」。
    #[test]
    fn bad_open_params_name_the_terminal_and_open_nothing() {
        let mut t = Terminal::new();
        let err = t
            .open(req(serde_json::json!({ "rows": "tall" })))
            .expect_err("尺寸给成字符串必须被拒");
        let text = err.to_string();
        assert!(
            matches!(err, ConnectorError::Invalid { .. }),
            "这是参数问题：{text}"
        );
        assert!(text.contains("terminal"), "错误要带上是谁的参数：{text}");
        assert!(!t.is_open(), "参数没过关就不该起 PTY");
    }

    /// `cwd` 真的落到子进程身上：`pwd` 打出来的就是它。
    ///
    /// 这条验的是 `open` → `spawn_in` 那一段接线，而不是 `spawn_in` 自己
    /// （后者在 `pty/unix.rs` 有单测）。少了这条，把 `cwd_ref` 传成 `None`
    /// 也照样全绿，而用户的终端会停在 worker 自己的目录上——他敲 `ls`
    /// 看到的是一个和自己工作区无关的目录。
    #[test]
    fn the_cwd_param_reaches_the_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().canonicalize().expect("canonicalize");
        let mut t = Terminal::new();
        t.open(OpenReq::new(
            "/tmp",
            serde_json::json!({ "shell": "/bin/sh", "cwd": real.to_string_lossy() }),
        ))
        .expect("open");
        t.on_data(&serde_json::json!({ "bytes": "pwd\nexit\n" }))
            .expect("type the command");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut text = String::new();
        let needle = real.to_string_lossy().into_owned();
        while std::time::Instant::now() < deadline && !text.contains(&needle) {
            for f in t.poll().expect("poll") {
                if let Some(s) = f.payload.get("bytes").and_then(|b| b.as_str()) {
                    text.push_str(s);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            text.contains(&needle),
            "cwd 没落到 shell 上，收不到 {needle}：{text}"
        );
    }

    /// `open` 的 `env` 追加到继承环境上，空串 `cwd` / 空串 `shell` 当「没给」。
    ///
    /// 三件事一起验是因为它们是同一个决定的三面：**缺省不是错误**。给了
    /// 空串的 `cwd` 去 `chdir("")` 会让面板一条命令都跑不起来，而空串的
    /// `shell` 是一个「页面上那个输入框是空的」的常态。
    #[test]
    fn an_empty_cwd_and_shell_fall_back_but_env_is_appended() {
        let key = "AFLEET_TERMINAL_TEST_KEY";
        let sentinel = format!("terminal-{}", std::process::id());
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({
            "shell": "/bin/sh",
            "cwd": "",
            "env": [[key, sentinel]],
        })))
        .expect("open");
        t.on_data(&serde_json::json!({ "bytes": "env\nexit\n" }))
            .expect("type the command");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut text = String::new();
        let needle = format!("{key}={sentinel}");
        while std::time::Instant::now() < deadline && !text.contains(&needle) {
            for f in t.poll().expect("poll") {
                if let Some(s) = f.payload.get("bytes").and_then(|b| b.as_str()) {
                    text.push_str(s);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(text.contains(&needle), "env 没交给 shell：{text}");
        assert!(
            text.contains("PATH="),
            "继承下来的环境必须还在（整体替换会让 TERM/PATH 一起消失）：{text}"
        );
        // 空串 `shell` 同样回落：`Shell` 字段给了空串，照样能起一个 shell。
        let mut blank = Terminal::new();
        blank
            .open(req(serde_json::json!({ "shell": "   " })))
            .expect("空串 shell 要回落到设备默认");
        assert!(blank.is_open(), "空串不该让面板打不开");
    }

    /// `drain` 把读线程队列里的东西挪进 `pending`，并且**按字节**把账减掉。
    ///
    /// 账不减的后果是「这次连接之后，下一条通道一上来就在等一个不存在的额度」：
    /// `wait_for_budget` 看着一个永远虚高的数，读线程再也不往下读——终端
    /// 面板表现为「输出到某一段就不动了」。
    ///
    /// **`queued` 为 `Some` 与 `None` 两条路各走一遍**：`detach` 会把 `queued`
    /// 摘成 `None`，而 `drain` 在那之后仍可能被调用一次（`poll` 与 `close`
    /// 之间有窗口），那条路不该 panic。
    #[test]
    fn drain_moves_bytes_into_pending_and_keeps_the_byte_account_in_step() {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let queued: QueueBytes = Arc::new(std::sync::atomic::AtomicUsize::new(11));
        tx.send(b"hello".to_vec()).expect("send");
        tx.send(b"world".to_vec()).expect("send");
        drop(tx);

        let mut t = Terminal::new();
        t.rx = Some(rx);
        t.queued = Some(Arc::clone(&queued));
        t.drain();
        assert_eq!(
            t.pending.len(),
            2,
            "两条 chunk 都该进 `pending`：{:?}",
            t.pending
        );
        assert_eq!(
            queued.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "账要按收到的字节减：11 - 5 - 5"
        );
        assert!(t.done, "发送端析构 = 输出到此为止");

        // `queued` 不在（`detach` 之后）时同样要把 chunk 收下，且不 panic。
        let (tx2, rx2) = mpsc::sync_channel::<Vec<u8>>(4);
        tx2.send(b"after-detach".to_vec()).expect("send");
        drop(tx2);
        let mut t2 = Terminal::new();
        t2.rx = Some(rx2);
        assert!(
            t2.queued.is_none(),
            "前提：这条用例要覆盖 `queued` 为 None 的那支"
        );
        t2.drain();
        assert_eq!(t2.pending.len(), 1, "没有账本也要收 chunk");
        assert!(t2.done);
    }

    /// 队列里还有字节时**不算跑完**：`done` 与 `pending` 缺一不可。
    ///
    /// 判「跑完」会让终端的最后几行永远发不出去——shell 退了，但它最后那句
    /// 输出还在 `pending` 里时，通道层收到 `finished() == true` 就会发
    /// `close_channel`，那一帧结果也就没有了。
    #[test]
    fn a_finished_shell_with_unsent_bytes_is_not_finished() {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
        tx.send(b"last words".to_vec()).expect("send");
        drop(tx);
        let mut t = Terminal::new();
        t.rx = Some(rx);
        t.queued = Some(Arc::new(std::sync::atomic::AtomicUsize::new(10)));

        // `drain` 之后：`done` 为真，但 `pending` 里还有一帧没发。
        t.drain();
        assert!(t.done, "发送端没了就是读完了");
        assert!(!t.finished(), "还有字节没发出去时不许自称跑完");

        // `poll` 真的把它泵出去，而且**一帧一条**（每个 credit 只值一帧）。
        let frames = t.poll().expect("poll");
        assert_eq!(frames.len(), 1, "一条 chunk 就是一帧：{frames:?}");
        assert_eq!(
            frames.first().and_then(|f| f.payload.get("bytes")),
            Some(&serde_json::json!("last words")),
            "载荷要原样交给页面"
        );
        assert!(t.finished(), "发完之后才算跑完");
        assert!(t.poll().expect("poll again").is_empty(), "没有新字节了");
    }

    /// 攒着的一帧**超过协议上限**时 `poll` 要把 `OutFrame::bytes` 的错误
    /// 原样交出去，而不是先发出半帧、或者干脆吞掉。
    ///
    /// 造法：直接把一条超大 chunk 塞进 `pending`（`OutFrame::bytes` 的判据是
    /// 1 MiB，见 `connector/mod.rs`）。这条臂在生产上真会被撞到：credit 预算
    /// 由 Hub 给，`credit_start` 可能大于 1 MiB，而 `READ_CHUNK` 是 16 KiB，
    /// 多块攒在 `pending` 里就可能越线。
    ///
    /// 判据是**错误是 `Internal` 且点明超限**——这条路径若静默丢帧，
    /// 用户看到的是「终端输出莫名少了一段」。
    #[test]
    fn poll_turns_an_oversized_chunk_into_a_visible_error() {
        let mut t = Terminal::new();
        let too_big = protocol::frame::MAX_FRAME_BYTES + 1;
        t.pending.push_back(vec![b'x'; too_big]);
        let err = t.poll().expect_err("单帧超限必须报错，而不是静默丢");
        assert!(matches!(err, ConnectorError::Internal { .. }), "got {err}");
        assert!(
            err.to_string().contains("1 MiB"),
            "错误要点明是帧上限，而不是一句空话：{err}"
        );
        assert!(
            t.pending.is_empty(),
            "越线的 chunk 已经被取走，不该留在队列里反复失败"
        );

        // 恰好等于上限是允许的：边界不能比协议更严，否则一块正常输出
        // 会变成一次可见故障。
        let mut ok = Terminal::new();
        ok.pending
            .push_back(vec![b'x'; protocol::frame::MAX_FRAME_BYTES]);
        let frames = ok.poll().expect("恰好 1 MiB 要能发出去");
        assert_eq!(frames.len(), 1, "一帧一条");
    }

    /// `start_reader` 在**没有 PTY** 时必须先说「没开」，而不是去 `dup_master`
    /// 一个空值、或者起一个没有源头的读线程。
    ///
    /// 正常路径上它在 `open` 里紧接着 `self.pty = Some(pty)` 被调用，所以这条
    /// 是「将来加一个新调用点（比如重连时只重起读线程）」的兜底：那里漏了
    /// 前置检查，得到的必须是一句人话，而不是一个生产代码里的 panic。
    #[test]
    fn start_reader_refuses_to_run_without_a_pty() {
        let mut t = Terminal::new();
        let err = t.start_reader().expect_err("没有 PTY 就不该起读线程");
        assert!(matches!(err, ConnectorError::Internal { .. }));
        assert!(err.to_string().contains("not open"), "got {err}");
        assert!(t.rx.is_none(), "没起来就不该留下接收端");
        t.drain();
        assert!(t.pending.is_empty(), "`rx` 为 None 时 `drain` 必须是空操作");
    }

    /// `start_reader` 在 **`pty` 有值、master 却已经关掉**时，要把 `dup_master`
    /// 的失败翻成 `Failed { op: "attach to terminal" }`。
    ///
    /// 这条臂与上一条不同：`self.pty` 是 `Some`，所以前置检查放行，真正的失败
    /// 来自 `dup_master`。生产上的意义是「重连时复用了一个已经被 `close` 的
    /// `Pty`」——那时起读线程会 `dup` 一个不存在的 fd，而错误必须说清是
    /// 「接不上终端」，不能是一句 errno，也不能落进 `Internal`（那会被当成
    /// 设备内部故障，而不是这一条通道接不上）。
    #[test]
    fn start_reader_reports_a_dup_failure_as_an_attach_failure() {
        let mut t = Terminal::new();
        t.open(req(serde_json::json!({ "shell": "/bin/sh" })))
            .expect("open");
        // 摘掉 master，但把 `Pty` 留在 `pty` 里：`is_open()` 为真，`dup_master`
        // 给 `Closed`。
        if let Some(pty) = t.pty.as_mut() {
            pty.close();
        }
        assert!(t.is_open(), "前提：`pty` 仍是 Some，前置检查会放行");

        let err = t.start_reader().expect_err("master 没了就 dup 不出来");
        match err {
            ConnectorError::Failed { op, detail } => {
                assert_eq!(op, "attach to terminal", "要指向「接上终端」这一步");
                assert!(
                    detail.contains("not open"),
                    "要带上是「终端没开」：{detail}"
                );
            }
            other => panic!("必须是 Failed 而不是别的变体：{other}"),
        }
        t.close();
    }
}
