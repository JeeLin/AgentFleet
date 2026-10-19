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

/// 超过这个字节数的文件**不整份载入**（`design-system.md` §3.14）。
///
/// 1 MiB 意味着 WebSocket 上的一个整帧（约束 4：每通道独立流控），而且在
/// 浏览器里编辑一兆文件没有意义——本项目的执行主体是 agent。
const MAX_READ_BYTES: u64 = 1024 * 1024;

/// 超大文件的预览策略：只取头尾各这么多字节。
const PREVIEW_EDGES: u64 = 4 * 1024;

/// `write` 的单次上限。与 [`MAX_READ_BYTES`] 同量级：写入没有「预览」
/// 这层缓冲，超大内容会一路进内存与落盘。
const MAX_WRITE_BYTES: usize = 1024 * 1024;

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
    /// 二进制内容。**与 [`WriteArgs::text`] 二选一**，都不给 = 写空文件。
    #[serde(default)]
    content_b64: Option<String>,
    /// UTF-8 文本。页面上的编辑器给文本，命令行工具给 base64。
    #[serde(default)]
    text: Option<String>,
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
                .unwrap_or_default()
                .cmp(b["name"].as_str().unwrap_or_default())
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
        let (bytes, truncated) = if md.len() > MAX_READ_BYTES {
            (preview(&path, &md), true)
        } else {
            (
                std::fs::read(&path).map_err(|e| ConnectorError::Failed {
                    op: "read file".to_string(),
                    detail: format!("{}: {e}", frame.path),
                })?,
                false,
            )
        };
        Ok(vec![OutFrame::new(preview_frame(
            &frame.path,
            &bytes,
            md.len(),
            truncated,
        ))])
    }

    /// 写一个文件。**父目录必须已存在**：自动建目录链会让一次打错的
    /// 路径在磁盘上造出一整条用户没要求过的树。
    fn write(&mut self, data: &serde_json::Value) -> Result<Vec<OutFrame>, ConnectorError> {
        let frame = InFrame::parse(data)?;
        let args: WriteArgs =
            serde_json::from_value(data.clone()).map_err(|e| ConnectorError::Invalid {
                detail: format!("bad files write data: {e}"),
            })?;
        let path = self.resolve(&frame.path)?;
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
                detail: format!("{} is a directory", frame.path),
            });
        }
        std::fs::write(&path, &content).map_err(|e| ConnectorError::Failed {
            op: "write file".to_string(),
            detail: format!("{}: {e}", frame.path),
        })?;
        Ok(vec![OutFrame::new(serde_json::json!({
            "op": "write",
            "path": frame.path,
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
/// 「原样字节」不成立（`protocol.md` §4 对 `exec_result` 是同一条理由）。
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
}
