//! 把 `vite build` 的产物写进编译期环境变量，供 `routes/assets.rs` 的
//! `include_dir!` 读取。
//!
//! **为什么不 `ServeDir`**：`DEVELOPMENT.md` §2.1 把「单二进制」定成硬需求
//! （「Rust 侧 include_dir! 编译期嵌入，运行时不需要 dist 目录」）。
//! 从磁盘服务 dist 会让部署多一个「二进制旁边得有 `packages/web/dist`」的
//! 隐含前提，而那正是自托管场景里最容易漏的一步。所以静态资产**只有一份
//! 来源**：编译期嵌入。
//!
//! **dist 不存在时不能硬失败**：`cargo test` / `cargo clippy` 在「只改 Rust、
//! 没跑前端构建」的工作树上必须照样能跑。缺产物时在 `OUT_DIR` 造一个可读的
//! 占位页并 `cargo:warning`，让二进制能起、页面能解释自己为什么是空的。
//! 这与「静默降级」不同：警告会出现在编译输出里，占位页也写明了原因。

use std::path::PathBuf;

/// 相对 `crates/hub-server/` 的 dist 位置。
const DIST_FROM_MANIFEST: &str = "../../packages/web/dist";

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

fn main() {
    let Some(manifest) = env_path("CARGO_MANIFEST_DIR") else {
        // Cargo 总会设它；真没有时也无从推算路径，让 include_dir! 自己报
        // 「AF_WEB_DIST 未定义」，那条错误比这里任何一句话都清楚。
        return;
    };
    let dist = manifest.join(DIST_FROM_MANIFEST);
    // 目录递归监听：vite 每次产物文件名都带内容哈希，目录级监听才不会漏掉
    // 「删旧文件 + 加新文件」（只监听 index.html 会漏掉 assets/*.js 的增删）。
    println!("cargo:rerun-if-changed=build.rs");

    if dist.join("index.html").is_file() {
        println!("cargo:rerun-if-changed={}", dist.display());
        println!("cargo:rustc-env=AF_WEB_DIST={}", dist.display());
        return;
    }

    let Some(out_dir) = env_path("OUT_DIR") else {
        return;
    };
    let stub_dir = out_dir.join("web-dist");
    if let Err(e) = std::fs::create_dir_all(&stub_dir) {
        println!("cargo:warning=cannot create the web-dist stub directory: {e}");
        return;
    }
    if let Err(e) = std::fs::write(stub_dir.join("index.html"), STUB_INDEX) {
        println!("cargo:warning=cannot write the web-dist stub page: {e}");
        return;
    }
    println!(
        "cargo:warning=packages/web/dist is missing; the Hub will serve a placeholder page. Run `bun run build` in packages/web."
    );
    println!("cargo:rustc-env=AF_WEB_DIST={}", stub_dir.display());
}

/// 占位页。**不用 `unwrap`/`expect` 读取任何东西**，也不引前端依赖：
/// 它是一个静态字符串，行为可预测，且不会因为构建环境不同而变化。
const STUB_INDEX: &str = r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="UTF-8" />
    <title>AgentFleet — web console not built</title>
    <style>
      body { font-family: ui-monospace, monospace; background: #111; color: #eee; padding: 3rem; }
      code { background: #222; padding: .1rem .35rem; }
    </style>
  </head>
  <body>
    <h1>Web console is not built</h1>
    <p>This binary was compiled without <code>packages/web/dist</code>.</p>
    <p>Build it with <code>bun run build</code> in <code>packages/web</code>, then rebuild the Hub.</p>
    <p>The API (<code>/api/*</code>) and device WebSocket (<code>/ws</code>) still work.</p>
  </body>
</html>
"#;
