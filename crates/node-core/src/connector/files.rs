//! files connector：工作区根内的浏览与读写（子任务 9）。
//!
//! **越界强制在 node 侧做**（约束 3）：Hub 不信任页面给的路径参数——
//! 页面能写的任何路径都可能是 `../../etc/shadow`，而 Hub 与设备之间只有
//! 一个进程边界，所以「判一下这个路径合不合法」必须落在这里。
//!
//! ## 载荷
//!
//! 下行（Hub → node，`channel_data`）：`{"op": "list" | "read" | "write", ...}`
//! - `list`：`{"path": "src"}`（**相对工作区根**；缺省 = 根本身）
//! - `read`：`{"path": "README.md"}`
//! - `write`：`{"path": "a.txt", "content_b64": "..."}` 或 `{"path": "a.txt", "text": "..."}`
//!
//! 上行（node → Hub，`channel_data`）：`{"op": ..., ...结果}`。
//!
//! ## 符号链接
//!
//! **列出时不跟随，只标记类型并给出目标**（§7.4 ①路：「树上的标记」）——
//! 隐藏会让用户以为树不完整，而用户会用终端里的 `ls` 看到它们，届时界面与
//! 终端不一致比越界反馈本身更伤信任。
//!
//! 但**进入时必须解析**：一个指向 `/etc` 的链接在树上标成 `⊘ 不可进入`，
//! 点进去却能读到内容，那不是标记而是漏判。所以 [`resolve_within`] 对
//! **存在的**目标做一次 `canonicalize`，链接目标落在根外就拒。

use std::path::{Component, Path, PathBuf};

use base64ct::{Base64, Encoding};
use serde::Deserialize;

use super::{Connector, ConnectorError, OpenReq, OutFrame};

/// `list` 最多回多少项。
///
/// **截断而不是报错**：一个 20 万文件的工作区不是错误，报错会让用户以为
/// 工作区坏了。截断时带 `truncated:true`，页面可以自己说「还有更多」。
const MAX_LIST_ENTRIES: usize = 1000;

/// 一帧的体积上限。**唯一真相源在协议层**（`protocol::frame::MAX_FRAME_BYTES`）：
/// node-core 已依赖 protocol，另立一份 1 MiB 字面量只会在协议改动时漂移一次。
///
/// Hub 侧在 tungstenite 层按同一个数判 `Capacity`，而读循环遇错即结束那条
/// 连接——**一帧超限的代价是这台设备上所有通道一起断**，不是「这一个面板
/// 打不开」。
const FRAME_BUDGET: usize = protocol::frame::MAX_FRAME_BYTES;

/// 信封的余量：`channel_data` 的 `c` 是 `ChannelData{channel_id, payload}`
/// 信封，帧本身还有 `v` / `seq` / `t` 等字段。
const ENVELOPE_MARGIN: usize = 1024;

/// 载荷预算：留给 `payload` 的字节数。
const PAYLOAD_BUDGET: usize = FRAME_BUDGET - ENVELOPE_MARGIN;

/// 超过这个字节数的文件**不整份载入**（`design-system.md` §3.14）。
///
/// 它由帧预算推出来而不是另写一份 1 MiB：载荷里既有 base64（4/3 膨胀）
/// 又有一份等长的 `text`，原始字节数必须比帧预算更小才有意义。
/// 而**原始字节数不是最终判据**——真正的判据是编码后的载荷大小。
const MAX_READ_BYTES: u64 = PAYLOAD_BUDGET as u64;

/// 超大文件的预览策略：只取头尾各这么多字节。
const PREVIEW_EDGES: u64 = 4 * 1024;

/// `write` 的单次上限（**原始字节**）。
///
/// 请求帧里放的是 base64（4/3 膨胀）或文本，内容本身必须比载荷预算小
/// 3/4 才装得下——否则 Hub 在 tungstenite 层就把这条连接摘了。
const MAX_WRITE_BYTES: usize = PAYLOAD_BUDGET * 3 / 4;

/// Files connector。开通道时 Hub 把会话工作区根塞进来，此后**不可变**。
///
/// 存的是**规范化后**的根（[`Path::canonicalize`] 的结果）：边界判据要
/// 落在真实文件系统上，而比较 `/tmp/ws` 与 `/tmp/ws/sub/..` 这种写法上的
/// 差异，前缀比较会得出错误结论。
#[derive(Debug)]
pub struct Files {
    root: Option<PathBuf>,
}

/// 工作区边界检查。**exec 复用它，不重复实现**（子任务 10）。
///
/// 三种逃逸构造在这里各自被拒：
/// 1. **`..` 逃逸**——词法向上走时栈空了就拒（而不是先 join 再 `canonicalize`，
///    那样 `..` 会把路径变合法之后再与根比较，多一次内核往返也更容易写错）；
/// 2. **绝对路径**——`Path::is_absolute` 一票否决，绝不「按相对路径解释」：
///    `/etc/passwd` 拼到根后面会变成 `/ws/etc/passwd`，那是一个**静默改变
///    用户意图**的答案，比报错坏得多；
/// 3. **符号链接指向界外**——对存在的部分做 `canonicalize` 后再判前缀。
///
/// `shown` 是**用户自己写的那个写法**，不是解析后的绝对路径：越界要**可见**
/// 又**不泄漏布局**（§7.4），而 `canonicalize` 的结果恰好会带上设备布局。
pub fn resolve_within(root: &Path, shown: &str) -> Result<PathBuf, ConnectorError> {
    let outside = || ConnectorError::OutsideWorkspace {
        shown: shown.to_string(),
    };
    let asked = Path::new(shown);
    if asked.is_absolute() {
        return Err(outside());
    }

    // ① 词法：逐段走，`..` 把根弹空就说明已经跑到根上面去了。
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for part in asked.components() {
        match part {
            // coverage: excluded — Unix 上 `RootDir` 必然让
            // `asked.is_absolute()` 为真、在 :97 就已经返回；`Prefix` 只在
            // Windows 出现（盘符 / UNC），本 crate 只在 Unix 目标上构建。
            // 留着这一臂是为了「将来若支持 Windows，越界判定不会漏掉盘符」。
            Component::Prefix(_) | Component::RootDir => return Err(outside()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err(outside());
                }
            }
            Component::Normal(seg) => stack.push(seg.to_os_string()),
        }
    }
    // 空串 / `.` / `a/..` 都落在根本身——那是**合法**的（`design-system.md`
    // 的面包屑每一层都要可见，根那一层不是非法输入）。
    if stack.is_empty() {
        return Ok(root.to_path_buf());
    }
    // `root.join(a).join(b)` 而不是 `stack.join`：后者要求索引切片，
    // 而生产代码里裸切片是一律禁止的（workspace lints deny）。
    let mut lexical = root.to_path_buf();
    for seg in &stack {
        lexical.push(seg);
    }

    // ③ 链接：对**最深的仍然存在的祖先**做 `canonicalize`。目标可能还不存在
    // （`write` 建新文件），那就只能查它的祖先——祖先已经是界外的情形，
    // 目标无论叫什么都在界外。
    let Some((mut real, tail)) = deepest_existing(&lexical) else {
        // coverage: excluded — `lexical` 由根的分段拼成，而根在 `open` 时
        // 已被 canonicalize 过（必然存在），所以向上走必然在根处停住。
        return Err(ConnectorError::internal(
            "workspace root is not reachable while resolving a path",
        ));
    };
    for seg in tail.iter().rev() {
        real.push(seg);
    }
    if !real.starts_with(root) {
        return Err(outside());
    }
    Ok(lexical)
}

/// 从 `path` 向上找第一个能被 `canonicalize` 的祖先，返回 `(它, 没走到的尾巴)`。
fn deepest_existing(path: &Path) -> Option<(PathBuf, Vec<std::ffi::OsString>)> {
    let mut probe = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return Some((real, tail));
        }
        let name = probe.file_name()?;
        let parent = probe.parent()?;
        tail.push(name.to_os_string());
        probe = parent;
    }
}

impl Files {
    pub fn new() -> Self {
        Self { root: None }
    }

    /// 已开着吗（工作区根拿到了且可用）。
    pub fn is_open(&self) -> bool {
        self.root.is_some()
    }

    /// 规范化后的工作区根（测试与排障用）。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn root_checked(&self) -> Result<&Path, ConnectorError> {
        self.root.as_deref().ok_or_else(|| ConnectorError::Invalid {
            detail: "files channel is not open".to_string(),
        })
    }

    /// 把用户给的路径解析成**真实文件系统上**的绝对路径。
    fn resolve(&self, shown: &str) -> Result<PathBuf, ConnectorError> {
        resolve_within(self.root_checked()?, shown)
    }
}

impl Connector for Files {
    fn open(&mut self, req: OpenReq) -> Result<Vec<OutFrame>, ConnectorError> {
        let root = req.root().ok_or_else(|| ConnectorError::WorkspaceRoot {
            detail: "no workspace root was given for this channel".to_string(),
        })?;
        // **根不存在要给一句人话**：裸的 `ENOENT` 让用户去查 inode，
        // 而真相通常是「会话的工作区记录指向一个已经删掉的目录」。
        let real = root
            .canonicalize()
            .map_err(|e| ConnectorError::WorkspaceRoot {
                detail: format!("{} is not usable: {e}", root.display()),
            })?;
        if !real.is_dir() {
            return Err(ConnectorError::WorkspaceRoot {
                detail: format!("{} is not a directory", root.display()),
            });
        }
        self.close();
        self.root = Some(real);
        Ok(Vec::new())
    }

    fn on_data(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        // 先确认开着：`data` 解不开时的错误会指向「帧格式」，
        // 而真实原因常常是这条通道早关了——后者才是用户要看的那句。
        self.root_checked()?;
        let op = data
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ConnectorError::Invalid {
                detail: "files data must carry a string \"op\"".to_string(),
            })?
            .to_string();
        match op.as_str() {
            "list" => self.list(data),
            "read" => self.read(data),
            "write" => self.write(data),
            other => Err(ConnectorError::Invalid {
                detail: format!("unknown files operation: {other}"),
            }),
        }
    }

    fn close(&mut self) {
        self.root = None;
    }
}

/// `list` / `read` / `write` 的公共入参。**只有路径可选**。
///
/// `op` 不在这里：它先被单独校验（并按它分派），留在结构体里只会让
/// 「op 已验过」这件事变成一个可以不一致的字段。
#[derive(Debug, Deserialize)]
struct InFrame {
    /// 相对工作区根。缺省按根本身处理（不是「空目录」）。
    #[serde(default)]
    path: String,
}

impl InFrame {
    fn parse(data: &serde_json::Value) -> Result<Self, ConnectorError> {
        serde_json::from_value(data.clone()).map_err(|e| ConnectorError::Invalid {
            detail: format!("bad files data: {e}"),
        })
    }
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    /// 相对工作区根。缺省按根本身处理（不是「空目录」）。
    #[serde(default)]
    path: String,
    /// 二进制内容。**与 [`WriteArgs::text`] 二选一**，都不给 = 写空文件。
    #[serde(default)]
    content_b64: Option<String>,
    /// UTF-8 文本。页面上的编辑器给文本，命令行工具给 base64。
    #[serde(default)]
    text: Option<String>,
}

impl WriteArgs {
    /// **一次解完**：`path` 与内容在同一个结构里。
    ///
    /// 分两次解（一次要 `path`、一次要内容）会得到两套不同的错误前缀，
    /// 而同一份坏载荷到底该报哪一句，取决于调用方先解了哪个。
    fn parse(data: &serde_json::Value) -> Result<Self, ConnectorError> {
        serde_json::from_value(data.clone()).map_err(|e| ConnectorError::Invalid {
            detail: format!("bad files write data: {e}"),
        })
    }
}

impl Files {
    /// 列一个目录。**按名排序**（`BTreeMap` 式稳定序）：同一个目录两次
    /// 打开的顺序不同，联调时的 diff 就读不懂了。
    fn list(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let frame = InFrame::parse(data)?;
        let path = self.resolve(&frame.path)?;
        let entries = std::fs::read_dir(&path).map_err(|e| ConnectorError::Failed {
            op: "list directory".to_string(),
            detail: format!("{}: {e}", frame.path),
        })?;
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut total = 0usize;
        for entry in entries {
            // coverage: excluded — `read_dir` 的迭代项本身出错要求目录在
            // 遍历期间被另一个进程改动，测试里不可确定地构造；`continue`
            // 让这种竞态不至于让整次列目录失败。
            let Ok(entry) = entry else { continue };
            total = total.saturating_add(1);
            if items.len() >= MAX_LIST_ENTRIES {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // **`symlink_metadata` 而不是 `metadata`**：后者会跟穿链接，
            // 于是一个指向 `/etc` 的链接在这里会伪装成一个普通目录。
            let md = entry.path().symlink_metadata().ok();
            let (kind, size) = match md.as_ref() {
                Some(m) => {
                    let ft = m.file_type();
                    let kind = if ft.is_symlink() {
                        "link"
                    } else if ft.is_dir() {
                        "dir"
                    } else if ft.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    (kind, if ft.is_file() { m.len() } else { 0 })
                }
                // coverage: excluded — 元数据读不到要求该项在 `read_dir`
                // 之后被另一个进程删掉，同样是竞态而非分支。
                None => ("unknown", 0),
            };
            let mut item = serde_json::Map::new();
            item.insert("name".into(), serde_json::Value::String(name));
            item.insert("kind".into(), serde_json::Value::String(kind.to_string()));
            item.insert("size".into(), serde_json::Value::from(size));
            item.insert("mtime_ms".into(), mtime_ms(md.as_ref()));
            // 目标给不给都无所谓：`read_link` 失败（竞态删除）只是一个
            // `null`，不该让整次列目录失败。
            if kind == "link" {
                item.insert("target".into(), link_target(&entry.path()));
            }
            items.push(serde_json::Value::Object(item));
        }
        items.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "list",
            "path": frame.path,
            "entries": items,
            "total": total,
            "truncated": total > MAX_LIST_ENTRIES,
        }))])
    }

    /// 读一个文件。**三种结论都要给**（§3.14：不许只显示「无法预览」）：
    /// 文本 / 二进制 / 超大只给头尾。
    ///
    /// **上限按编码后的载荷判**：载荷里有一份 base64（4/3 膨胀）加一份等长的
    /// `text`，原始字节数装得下不代表这一帧装得下。按原始字节判的形状是：
    /// 打开一个 500 KiB 的文本文件 → 单帧约 1.17 MiB → Hub 在 tungstenite
    /// 层摘掉这台设备的 WS → 它上面所有终端与文件面板一起断。
    fn read(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let frame = InFrame::parse(data)?;
        let path = self.resolve(&frame.path)?;
        let md = std::fs::metadata(&path).map_err(|e| ConnectorError::Failed {
            op: "read file".to_string(),
            detail: format!("{}: {e}", frame.path),
        })?;
        if md.is_dir() {
            return Err(ConnectorError::Failed {
                op: "read file".to_string(),
                detail: format!("{} is a directory", frame.path),
            });
        }
        let mut truncated = md.len() > MAX_READ_BYTES;
        let mut bytes = if truncated {
            preview(&path, &md)
        } else {
            std::fs::read(&path).map_err(|e| ConnectorError::Failed {
                op: "read file".to_string(),
                detail: format!("{}: {e}", frame.path),
            })?
        };
        let mut payload = preview_frame(&frame.path, &bytes, md.len(), truncated);
        if payload.to_string().len() > PAYLOAD_BUDGET {
            // 装不下就退回头尾预览：**截断必须说出来**，不能让用户以为
            // 读到的就是全文。
            truncated = true;
            bytes = preview(&path, &md);
            payload = preview_frame(&frame.path, &bytes, md.len(), truncated);
        }
        Ok(vec![OutFrame::new(payload)])
    }

    /// 写一个文件。**父目录必须已存在**：自动建目录链会让一次打错的
    /// 路径在磁盘上造出一整条用户没要求过的树。
    fn write(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let args = WriteArgs::parse(data)?;
        let path = self.resolve(&args.path)?;
        let content = match (args.content_b64, args.text) {
            (Some(b64), _) => {
                Base64::decode_vec(b64.as_str()).map_err(|e| ConnectorError::Invalid {
                    detail: format!("content_b64 is not valid base64: {e}"),
                })?
            }
            (None, Some(t)) => t.into_bytes(),
            (None, None) => Vec::new(),
        };
        if content.len() > MAX_WRITE_BYTES {
            return Err(ConnectorError::Invalid {
                detail: format!(
                    "refusing to write {} bytes; the limit is {MAX_WRITE_BYTES}",
                    content.len()
                ),
            });
        }
        if path.is_dir() {
            return Err(ConnectorError::Failed {
                op: "write file".to_string(),
                detail: format!("{} is a directory", args.path),
            });
        }
        std::fs::write(&path, &content).map_err(|e| ConnectorError::Failed {
            op: "write file".to_string(),
            detail: format!("{}: {e}", args.path),
        })?;
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "write",
            "path": args.path,
            "written": content.len(),
        }))])
    }
}

/// 一个符号链接的目标。**读不到就是 `null`**：竞态删除不该让整次列目录
/// 失败，而页面拿到 `null` 就把它标成「进不去」，那本来就是对的。
fn link_target(path: &Path) -> serde_json::Value {
    std::fs::read_link(path).map_or(serde_json::Value::Null, |p| {
        serde_json::Value::String(p.to_string_lossy().into_owned())
    })
}

/// mtime（Unix 毫秒）。**取不到就是 `null`**：时钟回拨等情况下
/// `duration_since` 会失败，编一个 0 出来会让「改过没有」这件事看不出来。
fn mtime_ms(md: Option<&std::fs::Metadata>) -> serde_json::Value {
    let Some(t) = md.and_then(|m| m.modified().ok()) else {
        return serde_json::Value::Null;
    };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => serde_json::Value::from(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        Err(_) => serde_json::Value::Null,
    }
}

/// 取一个超大文件的头尾各 [`PREVIEW_EDGES`] 字节。
///
/// **读不到就少给，不报错**：截断预览是「尽力给一个结论」，
/// 而这三条路各自失败只说明文件正在被改——那是下一次读取的事。
fn preview(path: &Path, md: &std::fs::Metadata) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut out: Vec<u8> = Vec::new();

    let head_len = md.len().min(PREVIEW_EDGES) as usize;
    let mut head = vec![0u8; head_len];
    if f.seek(SeekFrom::Start(0)).is_ok() && f.read_exact(&mut head).is_ok() {
        out.extend(head);
    }

    let tail_start = md.len().saturating_sub(PREVIEW_EDGES);
    let want = (md.len() - tail_start) as usize;
    let mut tail = vec![0u8; want];
    if f.seek(SeekFrom::Start(tail_start)).is_ok() && f.read_exact(&mut tail).is_ok() {
        out.extend(tail);
    }
    out
}

/// 读结果的载荷。**文本给 `text`，二进制给 `content_b64`**——JSON 只能装
/// 文本，而一次 `grep` 打到二进制文件上就会产出非法 UTF-8，强行转换会让
/// 「原样字节」不成立（exec 结果的 `stdout_b64` 是同一条理由）。
///
/// `kind` 是**结论**而不是猜测：页面照它选预览样式，不用自己嗅探。
fn preview_frame(shown: &str, bytes: &[u8], total: u64, truncated: bool) -> serde_json::Value {
    let looks_binary = bytes.contains(&0) || std::str::from_utf8(bytes).is_err();
    let kind = match (truncated, looks_binary) {
        (true, _) => "truncated",
        (false, true) => "binary",
        (false, false) => "text",
    };
    let mut map = serde_json::Map::new();
    map.insert("op".into(), "read".into());
    map.insert("path".into(), shown.into());
    map.insert("size".into(), total.into());
    map.insert("kind".into(), kind.into());
    map.insert("truncated".into(), truncated.into());
    map.insert("content_b64".into(), Base64::encode_string(bytes).into());
    // 文本另外给一份 `text`：页面要按行号渲染，而 base64 还得解一遍。
    if kind == "text" {
        map.insert(
            "text".into(),
            String::from_utf8_lossy(bytes).into_owned().into(),
        );
    }
    serde_json::Value::Object(map)
}

/// 「能用的默认值」必须真的存在：注册表按名造 connector 时走的就是
/// `Default`（`connector::create` → `Files::new`）。
impl Default for Files {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(root: &Path) -> Files {
        let mut f = Files::new();
        f.open(OpenReq::new(
            root.to_string_lossy().into_owned(),
            serde_json::json!({}),
        ))
        .expect("open");
        f
    }

    fn call(f: &mut Files, data: serde_json::Value) -> serde_json::Value {
        let mut out = f.on_data(&data).expect("on_data");
        let frame = out.pop().expect("one frame back");
        frame.payload
    }

    /// 一个真目录：外层临时目录 + 里面的工作区 + 旁边一个「界外」目录。
    fn sandbox() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn workspace(base: &tempfile::TempDir) -> PathBuf {
        let ws = base.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).expect("mkdir");
        std::fs::write(ws.join("src").join("a.txt"), b"hello\n").expect("write");
        std::fs::write(ws.join("README.md"), "readme body\n".as_bytes()).expect("write");
        std::fs::write(base.path().join("outside.txt"), b"secret\n").expect("write");
        ws
    }

    #[test]
    fn a_missing_workspace_root_reads_as_a_sentence_not_as_enoent() {
        let mut f = Files::new();
        let err = f.open(OpenReq::default()).expect_err("no root at all");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("no workspace root"), "got {err}");
    }

    #[test]
    fn a_root_that_is_not_there_is_reported_as_a_sentence() {
        let base = sandbox();
        let mut f = Files::new();
        let gone = base.path().join("deleted-workspace");
        let err = f
            .open(OpenReq::new(
                gone.to_string_lossy().into_owned(),
                serde_json::json!({}),
            ))
            .expect_err("must refuse");
        assert!(
            matches!(err, ConnectorError::WorkspaceRoot { .. }),
            "got {err}"
        );
        assert!(
            err.to_string().contains("not usable"),
            "要给出错在哪一步，而不是裸 errno：{err}"
        );
    }

    #[test]
    fn a_root_that_is_a_file_is_refused() {
        let base = sandbox();
        let file = base.path().join("not-a-dir");
        std::fs::write(&file, b"x").expect("write");
        let mut f = Files::new();
        let err = f
            .open(OpenReq::new(
                file.to_string_lossy().into_owned(),
                serde_json::json!({}),
            ))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not a directory"), "got {err}");
    }

    #[test]
    fn listing_the_workspace_root_names_entries_by_kind() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list" }));
        let entries = out["entries"].as_array().expect("entries");
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert_eq!(names, vec!["README.md", "src"], "{out}");
        assert_eq!(entries[1]["kind"], "dir");
        assert_eq!(entries[0]["kind"], "file");
        assert_eq!(entries[0]["size"], 12, "README.md 的字节数");
        assert!(entries[0]["mtime_ms"].is_number(), "mtime 要落到真实值上");
        assert!(!out["truncated"].as_bool().expect("truncated"));
    }

    #[test]
    fn listing_a_subdirectory_takes_the_path_relative_to_the_root() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list", "path": "src" }));
        let names: Vec<&str> = out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert_eq!(names, vec!["a.txt"], "{out}");
    }

    #[test]
    fn a_text_file_comes_back_as_text_with_the_conclusion_the_page_needs() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "read", "path": "README.md" }),
        );
        assert_eq!(out["kind"], "text");
        assert_eq!(out["text"], "readme body\n");
        assert_eq!(out["size"], 12);
        assert!(!out["truncated"].as_bool().expect("truncated"));
    }

    #[test]
    fn a_binary_file_comes_back_as_base64_rather_than_as_replacement_characters() {
        let base = sandbox();
        let ws = workspace(&base);
        std::fs::write(ws.join("blob.bin"), [0x00, 0xff, 0xfe, 0x01]).expect("write");
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "read", "path": "blob.bin" }),
        );
        assert_eq!(out["kind"], "binary");
        assert!(out["text"].is_null(), "二进制不该给 text 分支");
        let decoded =
            Base64::decode_vec(out["content_b64"].as_str().expect("b64")).expect("base64");
        assert_eq!(decoded, vec![0x00, 0xff, 0xfe, 0x01], "原样字节必须还原");
    }

    #[test]
    fn a_huge_file_is_previewed_from_both_ends_and_says_so() {
        let base = sandbox();
        let ws = workspace(&base);
        let body = vec![b'H'; (MAX_READ_BYTES + 10_000) as usize];
        std::fs::write(ws.join("big.txt"), &body).expect("write");
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "read", "path": "big.txt" }),
        );
        assert_eq!(out["kind"], "truncated");
        assert!(out["truncated"].as_bool().expect("truncated"));
        assert_eq!(out["size"], body.len());
        let bytes = Base64::decode_vec(out["content_b64"].as_str().expect("b64")).expect("base64");
        assert!(
            bytes.len() <= (PREVIEW_EDGES * 2) as usize,
            "超大文件整份下来了：{} 字节",
            bytes.len()
        );
        assert!(bytes.iter().all(|b| *b == b'H'), "头尾都该是文件本体");
    }

    /// **上限按编码后的载荷判**：一个原始大小在预算之内、但编码后装不下的
    /// 文件，必须退回头尾预览。
    ///
    /// 形状就是那个真实故障：点开一个 500 KiB 的文本文件 → 一帧约 1.17 MiB
    /// → Hub 在 tungstenite 层摘掉这台设备的 WS → 它上面所有 terminal /
    /// files / exec 通道一起断。按原始字节判的形状里，这个文件**装得下**，
    /// 于是它被整份读出来、整份发出去。
    #[test]
    fn a_file_that_fits_by_bytes_but_not_after_encoding_is_previewed() {
        let base = sandbox();
        let ws = workspace(&base);
        // 原始大小在 `MAX_READ_BYTES` 之内，但 base64（4/3）加一份等长的
        // `text` 装不下载荷预算。
        let body = "x".repeat((MAX_READ_BYTES - 4096) as usize);
        std::fs::write(ws.join("fat.txt"), body.as_bytes()).expect("write");
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "read", "path": "fat.txt" }),
        );
        assert_eq!(out["kind"], "truncated", "装不下就必须截断并说出来");
        assert!(out["truncated"].as_bool().expect("truncated"));
        let encoded = out.to_string().len();
        assert!(
            encoded <= PAYLOAD_BUDGET,
            "编码后仍有 {encoded} 字节，超出载荷预算 {PAYLOAD_BUDGET}"
        );
    }

    #[test]
    fn writing_a_file_puts_exactly_those_bytes_on_disk() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "write", "path": "src/b.txt", "text": "写进去\n" }),
        );
        assert_eq!(out["written"], 10, "写的是字节数不是字符数");
        let back = std::fs::read_to_string(ws.join("src").join("b.txt")).expect("read back");
        assert_eq!(back, "写进去\n");
    }

    #[test]
    fn writing_binary_content_uses_the_base64_field() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let payload = Base64::encode_string(&[0x00, 0xff]);
        call(
            &mut f,
            serde_json::json!({ "op": "write", "path": "bin.dat", "content_b64": payload }),
        );
        assert_eq!(
            std::fs::read(ws.join("bin.dat")).expect("read"),
            vec![0x00, 0xff]
        );
    }

    /// 写一个空文件：两个字段都不给**是合法的**，页面上「新建空文件」
    /// 就是这么点的。
    #[test]
    fn a_write_with_neither_field_creates_an_empty_file() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let out = call(
            &mut f,
            serde_json::json!({ "op": "write", "path": "empty.txt" }),
        );
        assert_eq!(out["written"], 0);
        assert_eq!(
            std::fs::read(ws.join("empty.txt")).expect("read"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn writing_over_a_directory_says_so_instead_of_replacing_it() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "write", "path": "src", "text": "x" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("is a directory"), "got {err}");
        assert!(ws.join("src").is_dir(), "目录还在");
    }

    #[test]
    fn writing_into_a_directory_that_is_not_there_does_not_create_the_whole_chain() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(
                &serde_json::json!({ "op": "write", "path": "no/such/dir/a.txt", "text": "x" }),
            )
            .expect_err("must refuse");
        assert!(err.to_string().contains("write file"), "got {err}");
        assert!(!ws.join("no").exists(), "不该顺手建出一整条目录链");
    }

    #[test]
    fn a_write_over_the_size_limit_is_refused_before_touching_the_disk() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        let err = f
            .on_data(&serde_json::json!({ "op": "write", "path": "big.txt", "text": big }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("refusing to write"), "got {err}");
        assert!(!ws.join("big.txt").exists());
    }

    #[test]
    fn content_that_is_not_base64_is_refused_by_name() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "write", "path": "a.txt", "content_b64": "!!!!" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not valid base64"), "got {err}");
    }

    /// **越界构造 ①**：`..` 逃逸。判据是**真文件系统**上的那一次读：
    /// 界外文件真的存在，如果边界检查失守，这一下会读到它。
    #[test]
    fn a_dot_dot_that_climbs_out_of_the_workspace_is_refused() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let outside = base.path().join("outside.txt");
        assert!(outside.exists(), "判据要落在真实文件上");
        for asked in [
            "..",
            "../outside.txt",
            "src/../../outside.txt",
            "a/../../..",
        ] {
            let err = f
                .on_data(&serde_json::json!({ "op": "read", "path": asked }))
                .expect_err("must refuse");
            assert!(
                matches!(err, ConnectorError::OutsideWorkspace { .. }),
                "{asked} 越界却没被拒: {err}"
            );
            assert!(
                err.to_string().contains("outside the workspace"),
                "got {err}"
            );
        }
    }

    /// **越界构造 ②**：绝对路径。
    ///
    /// **两种写法都要拒**：工作区自己的绝对路径（会绕过一切相对解析）
    /// 与界外的绝对路径。而最容易被写成「能跑」的那个是**同一个工作区的
    /// 绝对路径**——它确实在工作区内，但接受它等于承认页面可以指定根，
    /// 于是根只是页面上一个可改的输入。
    #[test]
    fn an_absolute_path_is_refused_even_when_it_points_inside_the_workspace() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let inside_abs = ws.join("README.md");
        let outside_abs = base.path().join("outside.txt");
        for asked in [inside_abs, outside_abs] {
            let shown = asked.to_string_lossy().into_owned();
            let err = f
                .on_data(&serde_json::json!({ "op": "read", "path": shown }))
                .expect_err("must refuse");
            assert!(
                matches!(err, ConnectorError::OutsideWorkspace { .. }),
                "{shown} 越界却没被拒: {err}"
            );
        }
    }

    /// **越界构造 ③**：符号链接指向界外。
    ///
    /// 真 `symlink(2)`：判据不能是 mock——只有真的解析过才能证明
    /// 「树上一个可点的项，点进去读不到界外的东西」。
    #[test]
    fn a_symlink_pointing_outside_the_workspace_is_refused() {
        let base = sandbox();
        let ws = workspace(&base);
        let secret = base.path().join("outside.txt");
        std::os::unix::fs::symlink(&secret, ws.join("leak.txt")).expect("symlink");
        std::os::unix::fs::symlink(base.path(), ws.join("leakdir")).expect("symlink");
        let mut f = open(&ws);

        // 树里**看得见**它（§7.4 ①路：标记而不是隐藏）
        let out = call(&mut f, serde_json::json!({ "op": "list" }));
        let leak = out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["name"] == "leak.txt")
            .cloned()
            .expect("链接不该被隐藏");
        assert_eq!(leak["kind"], "link");
        assert!(
            leak["target"]
                .as_str()
                .is_some_and(|t| t.ends_with("outside.txt")),
            "链接要给出目标，否则用户没法自己判断：{leak}"
        );

        // 但**读不到**它
        for asked in ["leak.txt", "leakdir"] {
            let err = f
                .on_data(&serde_json::json!({ "op": "read", "path": asked }))
                .expect_err("must refuse");
            assert!(
                matches!(err, ConnectorError::OutsideWorkspace { .. }),
                "{asked} 越界却没被拒: {err}"
            );
        }
    }

    /// **第四种构造**：目录里的一级链接指向界内的目录——它**应该**能进，
    /// 否则就把「链接」与「越界」混成同一件事，用户连正常的
    /// `node_modules` 软链都点不开。
    #[test]
    fn a_symlink_that_stays_inside_the_workspace_is_followed() {
        let base = sandbox();
        let ws = workspace(&base);
        std::os::unix::fs::symlink(ws.join("src"), ws.join("alias")).expect("symlink");
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list", "path": "alias" }));
        let names: Vec<&str> = out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert_eq!(names, vec!["a.txt"], "界内链接要能进：{out}");
    }

    /// 越界**对写也一样**：只堵读不堵写等于留了一个往工作区外写文件的洞。
    #[test]
    fn writing_through_an_escaping_path_is_refused() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "write", "path": "../escaped.txt", "text": "x" }))
            .expect_err("must refuse");
        assert!(
            matches!(err, ConnectorError::OutsideWorkspace { .. }),
            "got {err}"
        );
        assert!(!base.path().join("escaped.txt").exists());
    }

    /// 越界的错误**可读但不泄漏布局**（§7.4）：`shown` 是用户自己写的
    /// 那个写法，`to_string()` 里不该出现设备上的绝对路径。
    #[test]
    fn the_boundary_error_shows_the_users_own_spelling_not_the_devices_layout() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let secret = base.path().join("outside.txt");
        std::os::unix::fs::symlink(&secret, ws.join("leak.txt")).expect("symlink");
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": "leak.txt" }))
            .expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("leak.txt"), "越界要看得见：{text}");
        let layout = ws.to_string_lossy().into_owned();
        assert!(!text.contains(&layout), "错误里泄漏了设备布局：{text}");
    }

    /// 根目录本身**是**合法的路径（面包屑的根那一层，`design-system.md`
    /// §7.4 ②路要求每一层都可见）。
    #[test]
    fn the_workspace_root_itself_is_a_legal_path() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        for asked in ["", ".", "src/.."] {
            let out = call(&mut f, serde_json::json!({ "op": "list", "path": asked }));
            assert_eq!(
                out["entries"].as_array().map(Vec::len),
                Some(2),
                "{asked} -> {out}"
            );
        }
    }

    #[test]
    fn a_missing_path_is_a_readable_error_not_a_bare_errno() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": "nope.txt" }))
            .expect_err("must fail");
        assert!(err.to_string().contains("read file"), "got {err}");
        assert!(
            err.to_string().contains("nope.txt"),
            "要说清是哪个路径：{err}"
        );
    }

    #[test]
    fn reading_a_directory_says_it_is_a_directory() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": "src" }))
            .expect_err("must fail");
        assert!(err.to_string().contains("is a directory"), "got {err}");
    }

    /// 目录列表超过上限时**截断并明说**：报错会让用户以为工作区坏了。
    #[test]
    fn a_very_large_directory_is_truncated_and_says_so() {
        let base = sandbox();
        let ws = workspace(&base);
        let inside = ws.join("many");
        std::fs::create_dir(&inside).expect("mkdir");
        for i in 0..(MAX_LIST_ENTRIES + 5) {
            std::fs::write(inside.join(format!("f{i:06}")), b"").expect("write");
        }
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list", "path": "many" }));
        assert_eq!(
            out["entries"].as_array().map(Vec::len),
            Some(MAX_LIST_ENTRIES)
        );
        assert_eq!(out["total"], MAX_LIST_ENTRIES + 5);
        assert!(out["truncated"].as_bool().expect("truncated"));
    }

    /// 悬空链接**列得出来、标成链接**：它不指向任何可读的东西，但隐藏它
    /// 会让用户以为树不完整（`design-system.md` §7.4 明确否决了隐藏）。
    #[test]
    fn a_dangling_symlink_is_listed_as_a_link_with_a_null_target() {
        let base = sandbox();
        let ws = workspace(&base);
        std::os::unix::fs::symlink(ws.join("does-not-exist"), ws.join("broken")).expect("symlink");
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list" }));
        let names: Vec<&str> = out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(names.contains(&"broken"), "悬空链接仍然列得出来：{out}");
        assert_eq!(
            out["total"],
            names.len(),
            "total 要与实际列出的项一致：{out}"
        );
    }

    /// `kind` 那一档不是文件/目录/链接的东西（管道、socket）。
    #[test]
    fn a_fifo_is_reported_as_other_rather_than_as_a_file() {
        let base = sandbox();
        let ws = workspace(&base);
        let fifo = ws.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).expect("cstring");
        // coverage: excluded — mkfifo 只可能因 umask/路径/已存在而失败，
        // 这里传的是刚由 tempfile 建出的目录下的新名字。
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo 失败，测不了这一档");
        let mut f = open(&ws);
        let out = call(&mut f, serde_json::json!({ "op": "list" }));
        let pipe = out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["name"] == "pipe")
            .cloned()
            .expect("fifo 要列出来");
        assert_eq!(pipe["kind"], "other");
        assert_eq!(pipe["size"], 0);
    }

    #[test]
    fn data_without_an_op_names_what_it_should_have_carried() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "path": "x" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("\"op\""), "got {err}");
    }

    #[test]
    fn an_unknown_op_lists_what_this_connector_actually_does() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "delete", "path": "x" }))
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("unknown files operation: delete"),
            "got {err}"
        );
    }

    #[test]
    fn data_on_a_closed_files_channel_says_the_channel_is_gone() {
        let mut f = Files::new();
        let err = f.on_data(&serde_json::json!({})).expect_err("must refuse");
        assert!(err.to_string().contains("not open"), "got {err}");
        assert!(!f.is_open());
    }

    #[test]
    fn reopening_moves_to_the_new_root_and_closing_twice_is_allowed() {
        let base = sandbox();
        let first = workspace(&base);
        let mut f = open(&first);
        let second = base.path().join("ws2");
        std::fs::create_dir(&second).expect("mkdir");
        std::fs::write(second.join("only.txt"), b"x").expect("write");
        f.open(OpenReq::new(
            second.to_string_lossy().into_owned(),
            serde_json::json!({}),
        ))
        .expect("reopen");
        let out = call(&mut f, serde_json::json!({ "op": "list" }));
        assert_eq!(
            out["entries"]
                .as_array()
                .and_then(|e| e.first())
                .and_then(|e| e["name"].as_str()),
            Some("only.txt"),
            "重开后根必须换掉：{out}"
        );
        f.close();
        f.close();
        assert!(!f.is_open());
        assert!(f.root().is_none());
    }

    #[test]
    fn the_registry_routes_the_files_name_here() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut c = crate::connector::create(crate::connector::names::FILES).expect("registered");
        c.open(OpenReq::new(
            ws.to_string_lossy().into_owned(),
            serde_json::json!({}),
        ))
        .expect("open through the registry");
    }

    /// 一个标了 `Prefix` / `RootDir` 的写法（`//host/share`）——它既不是
    /// 绝对路径的常见形态，也不是 `..`，值得单独钉住。
    #[test]
    fn a_double_slash_prefix_is_refused_like_any_other_escaping_form() {
        let base = sandbox();
        let ws = workspace(&base);
        let f = open(&ws);
        let err = resolve_within(f.root().expect("root"), "//etc/passwd").expect_err("must refuse");
        assert!(
            matches!(err, ConnectorError::OutsideWorkspace { .. }),
            "got {err}"
        );
    }

    /// 列一个目录失败要说清**是哪一步**：`read_dir` 拿 `ENOTDIR` / `EACCES`
    /// 回来时，一句 `list directory` 加用户自己写的那个路径就够了定位。
    #[test]
    fn listing_a_path_that_is_not_a_directory_says_so_by_name() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "list", "path": "README.md" }))
            .expect_err("must fail");
        assert!(err.to_string().contains("list directory"), "got {err}");
        assert!(
            err.to_string().contains("README.md"),
            "要说清是哪个路径：{err}"
        );
    }

    /// `read` 失败要指名 `read file`：与 `list directory` 分开，用户才知道
    /// 是哪一步塌的。
    #[test]
    fn reading_an_unreadable_path_names_the_read_step() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": "src" }))
            .expect_err("must fail");
        assert!(err.to_string().contains("read file"), "got {err}");
    }

    /// 载荷解不开时要说出**是 files 的数据坏了**：同一个坏 JSON 在
    /// terminal / exec 下含义完全不同，serde 自己的错误不含「这是给谁的」。
    #[test]
    fn a_payload_of_the_wrong_shape_names_the_files_connector() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": 42 }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("bad files data"), "got {err}");
    }

    /// `write` 的解错误要有自己的前缀：与 `read` 分开，否则同一份坏载荷
    /// 报哪一句取决于调用方先解了哪个。
    #[test]
    fn a_write_payload_of_the_wrong_shape_names_the_write_step() {
        let base = sandbox();
        let ws = workspace(&base);
        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "write", "path": "a.txt", "content_b64": 7 }))
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("bad files write data"),
            "got {err}"
        );
    }

    /// `Default` 是注册表按名造 connector 时走的那条路（`create` → `new`）。
    /// 一个只能 `new()` 的类型进不了那张表，所以默认值必须真的给出
    /// 一个「没开但能开」的实例。
    #[test]
    fn the_default_files_connector_is_closed_and_openable() {
        let mut f = Files::default();
        assert!(!f.is_open(), "默认实例不该凭空占一个工作区");
        assert!(f.root().is_none());
        // 没开着就发数据：错误要指向「通道没开」，而不是 panic
        let err = f
            .on_data(&serde_json::json!({ "op": "list" }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not open"), "got {err}");
    }

    /// mtime 取不到就是 `null`，**不编一个 0 出来**。
    ///
    /// 编 0 会让「这个文件改过没有」永远显示成「1970-01-01，没改过」，
    /// 而真相是「读不到」——那是两回事，页面给的结论也该不一样。
    #[test]
    fn an_unreadable_mtime_is_null_not_a_fabricated_epoch() {
        // `None` 元数据 → 直接 `null`（`mtime_ms` 的第一条臂）。
        assert_eq!(mtime_ms(None), serde_json::Value::Null);
        // 有元数据但 `modified()` 失败同样落 `null`——这里用一份真实
        // 元数据走成功那条路，确认「取得到时给的是数字而不是 null」，
        // 否则这条断言会在两条臂都返回 null 时照样是绿的。
        let base = sandbox();
        let ws = workspace(&base);
        let md = std::fs::metadata(ws.join("README.md")).expect("metadata");
        assert!(
            mtime_ms(Some(&md)).is_u64(),
            "取得到 mtime 时必须给数字：{:?}",
            mtime_ms(Some(&md))
        );
    }

    /// `read` 只对**目录**做豁免：其余「元数据有、但字节读不出来」的
    /// 情况必须把 `read file` 说清楚，并带上**用户写的那个路径**。
    ///
    /// 这条覆盖 `std::fs::read` 的失败臂。触发物选的是 `AF_UNIX`
    /// **监听套接字**：
    ///   - `stat` 成功（所以绕开前面「路径不在」那一条）、是常规项而不是
    ///     目录（所以绕开 `is_dir` 豁免）——两个前置都满足才可能走到这里；
    ///   - `open` 却以 `ENXIO` 失败，因为内核拒绝给 socket 的 inode 一个
    ///     字节流。**这与 uid 无关**：`chmod 000` 在 root 下形同虚设
    ///     （root 读得穿），那样这条测试会在 CI 的 root 容器里静默变成
    ///     空转，却仍然显示绿色。
    ///
    /// 判据是「错误归属读这一步且指名文件」，而不是「errno 是几」。
    #[test]
    fn reading_a_path_that_has_no_bytes_names_the_read_step() {
        let base = sandbox();
        let ws = workspace(&base);
        let path = ws.join("sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        // 先自证两个前置：可 stat、且不是目录。任何一条不成立，下面
        // 的错误就会来自更早的分支，这条测试就测错了地方。
        let md = std::fs::metadata(&path).expect("metadata");
        assert!(!md.is_dir(), "socket 不是目录");
        assert!(
            std::fs::read(&path).is_err(),
            "前提是「字节读不出来」，否则测不到这条臂"
        );

        let mut f = open(&ws);
        let err = f
            .on_data(&serde_json::json!({ "op": "read", "path": "sock" }))
            .expect_err("must fail");
        assert!(err.to_string().contains("read file"), "got {err}");
        assert!(err.to_string().contains("sock"), "要说清是哪个文件：{err}");
        drop(listener);
    }

    /// mtime 早于 Unix 纪元 → `null`，**不编一个 0 出来**。
    ///
    /// 编 0 会让「改过没有」永远显示成「1970-01-01」，而真相是「这个
    /// 时间戳说不出来」——那是两回事。时钟回拨 / 挂载点给出负时间戳时
    /// 会落到这条臂。
    #[test]
    fn an_mtime_before_the_epoch_is_null_not_a_fabricated_zero() {
        let base = sandbox();
        let ws = workspace(&base);
        let path = ws.join("README.md");
        // `utimensat` 是把 mtime 设成任意值（含负数）的唯一手段：
        // `File::set_times` 只接受 `SystemTime`，而 `UNIX_EPOCH - 1s`
        // 在 `SystemTime` 的算术里构造不出来。
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("cstring");
        let times = [
            libc::timespec {
                tv_sec: -1,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: -1,
                tv_nsec: 0,
            },
        ];
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(
            rc,
            0,
            "utimensat 失败：{:?}",
            std::io::Error::last_os_error()
        );

        let md = std::fs::metadata(&path).expect("metadata");
        assert!(
            md.modified().is_ok(),
            "前提是「modified() 拿得到」，否则测的是另一条臂"
        );
        assert_eq!(mtime_ms(Some(&md)), serde_json::Value::Null);
    }

    /// `preview` 读不到就**少给而不报错**：它是「尽力给一个结论」，
    /// 而读失败只说明文件正在被改——那是下一次读取的事。
    #[test]
    fn preview_of_a_file_that_is_gone_comes_back_empty_not_failed() {
        let base = sandbox();
        let ws = workspace(&base);
        let path = ws.join("README.md");
        let md = std::fs::metadata(&path).expect("metadata");
        // 先确认「文件在」时给得出字节：否则「空」可能是别的原因造成的。
        assert!(
            !preview(&path, &md).is_empty(),
            "文件在的时候 preview 必须给出字节"
        );
        std::fs::remove_file(&path).expect("remove");
        assert!(
            preview(&path, &md).is_empty(),
            "文件没了要返回空，而不是报错"
        );
    }
}
