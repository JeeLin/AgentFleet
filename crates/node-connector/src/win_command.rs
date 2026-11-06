//! Win32 命令行的**纯逻辑**部分：引号拼接、环境块编码、UTF-16 转换。
//!
//! ## 为什么单独一个文件
//!
//! 这三件事在 `CreateProcessW` 之前做完，**不需要任何 Win32 调用、不需要
//! `unsafe`**——它们只是字符串处理。但它们的正确性直接决定子进程能不能拿到
//! 正确的参数：拼错一个引号，`"a b"` 会被拆成 `a` 和 `b` 两个参数，而报错
//! 出现在子进程那边（「找不到文件 a」），排查方向完全错。
//!
//! 把它们从 `pty/windows.rs` 与 `connector/exec_windows.rs` 里提出来，
//! 有两个实在的好处：
//!
//! 1. **本地能跑测试**。Windows 的系统调用只能在 CI 的 windows runner 上验，
//!    但这几段在 Linux 上就能测——`cfg(test)` 不依赖平台，`Vec<u16>` 也只是
//!    个字节数组。于是「只能靠 CI 验」的面积从「整个 spawn 流程」缩到
//!    「三个系统调用本身」。
//! 2. **只有一份引号规则**。原先两个文件各抄了一遍，将来必然漂移一次，
//!    而漂移的那次就是「终端里跑得对、exec 里跑得错」这种最难查的 bug。
//!
//! ## 引号规则从哪来
//!
//! 不是我们自己发明的：`CommandLineToArgvW` 与 MSVC 运行时的解析规则一致，
//! 也就是 Rust 标准库 `std::process::Command` 在 Windows 上做的事。规则三条：
//!
//! - 含空格 / 制表符 / 引号的参数整段加引号；
//! - 参数内部的 `"` 写成 `\"`，且它**前面**的连续反斜杠要翻倍；
//! - 参数**结尾**的连续反斜杠也要翻倍，否则它们会把收尾引号转义掉。
//!
//! 第三条最容易漏，也最隐蔽：`C:\dir\` 不加处理会变成 `"C:\dir\"`，
//! 那个 `\"` 被解析成「一个字面引号」，收尾引号就没了。

/// 把 `&str` 编成以 NUL 结尾的 UTF-16（Win32 的 `W` 系列 API 收这个）。
///
/// **不用 `OsStrExt::encode_wide`**：那个 trait 只在 Windows 上存在，用了它
/// 这段就没法在 Linux 上编译，也就没法在本地跑测试——而本文件存在的全部
/// 理由就是「这类纯逻辑要能本地验」。`str::encode_utf16` 给出完全相同的
/// 结果，且在哪个平台都能编。
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// 按 MSVC 运行时 / `CommandLineToArgvW` 共同的规则给单个参数加引号。
///
/// **判据与 Rust 的 `Quote::Auto` 完全一致**（`std::process::Command` 在
/// Windows 上就是这么做的），只有两条：
///
/// - 含空格或制表符 → 必须加引号（它们是硬分隔符，引号外没有转义）；
/// - 空参数 → 必须加引号，否则它在命令行里**整段消失**（连续两个空格）。
///
/// 两条容易写错的边界：
///
/// 1. **结尾反斜杠不触发加引号**。规则 3 说得明白：「n 个反斜杠后面不跟引号，
///    就是 n 个字面反斜杠」——不加引号时它们不会被当成转义，`C:\dir\` 原样
///    传过去完全正确。加引号反而是画蛇添足，而且会让字节与 Rust 的命令行
///    不一致（有些子程序对「有没有被引号包住」是敏感的）。
/// 2. **但一旦加了引号，结尾反斜杠就必须翻倍**。否则 `"C:\dir\"` 里那个 `\"`
///    会被解析成「一个字面引号」，收尾引号消失、后面的参数全被吞掉。
///    所以翻倍逻辑在 `if quote` 分支里，而不是判据里。
///
/// 第二条是「结果对、字节不对」之外的真 bug 来源，所以两个判断必须分开写。
pub(crate) fn quote(arg: &str) -> String {
    // **两个判断必须分开**：
    //   `quote` —— 要不要在外面包一对引号（只由空格 / 制表符 / 空串决定）；
    //   `escape` —— 要不要把内部 `"` 转义成 `\"`（**永远要**）。
    //
    // 把转义也塞进 `if quote` 是错的：`a"b` 不需要外层引号，但那个 `"` 仍
    // 必须写成 `\"`，否则解析器把它当分隔符，一个参数变成两段。Rust 的
    // `append_arg` 正是这么分的（`Quote::Auto` 只影响首尾两个 `push('"')`）。
    let quote = arg.is_empty() || arg.contains([' ', '\t']);

    let mut out = String::with_capacity(arg.len() + 2);
    if quote {
        out.push('"');
    }
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // n 个反斜杠 + 引号 → 2n+1 个反斜杠 + 引号：解析回来是
                // n 个反斜杠 + 一个字面引号。
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            c => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    if quote {
        // 结尾反斜杠翻倍（n → 2n）：不翻倍的话它们会转义掉收尾引号。
        for _ in 0..backslashes {
            out.push('\\');
        }
        out.push('"');
    }
    out
}

/// 整条命令行：程序名 + 已加引号的参数，空格分隔。
///
/// **程序名也加引号**：`C:\Program Files\nodejs\afcli.exe` 不加引号会被
/// 拆成两段，而 `CreateProcessW` 会报「找不到文件 C:\Program」。
pub(crate) fn command_line(program: &str, args: &[String]) -> String {
    let mut parts = vec![quote(program)];
    parts.extend(args.iter().map(|a| quote(a)));
    parts.join(" ")
}

/// 环境块：`KEY=VALUE\0KEY=VALUE\0...\0\0`（UTF-16）。
///
/// 语义与 Unix 侧 `spawn_in` 一致：**在继承的环境上追加**（`extra` 覆盖同名键），
/// 不整体替换。整体替换会让 `PATH` / `SystemRoot` 消失，而没有 `SystemRoot`
/// 的进程在 Windows 上连 DNS 都查不了。
///
/// 含 NUL 的项直接跳过：环境块格式不允许转义，塞进去只会把后面的项截断。
/// 这与 Unix 侧「NUL 即报错」不同——那里报错是安全的，这里的替代方案是
/// 「少一条环境变量」而不是「整次启动失败」。
pub(crate) fn environment_block(extra: &[(String, String)]) -> Vec<u16> {
    let mut merged: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in extra {
        match merged.iter_mut().find(|(mk, _)| mk == k) {
            Some(slot) => slot.1 = v.clone(),
            None => merged.push((k.clone(), v.clone())),
        }
    }
    let mut block = Vec::new();
    for (k, v) in merged {
        // 跳过含 NUL 的项：环境块是 NUL 分隔的，键或值里混进 NUL 会把
        // 后面整段截断。
        //
        // **键里含 `=` 也要跳过**：环境块里 `KEY=VALUE` 的分隔符是**第一个**
        // `=`，所以键 `A=B` 拼出来是 `A=B=1`，而 `CreateProcessW` 读到的是
        // 键 `A`、值 `B=1`——调用方以为自己设了 `A=B`，实际改的是 `A`。这不是
        // 注入（`=` 逃不出键名），但是一次**静默的错键**，比直接报错更难查。
        // 环境变量名本来就不允许含 `=`，这种输入只可能是调用方写错了。
        if k.contains('\0') || v.contains('\0') || k.contains('=') {
            continue;
        }
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// 参数里出现 NUL 会让 Win32 截断命令行，必须在起进程**之前**挡住。
///
/// 抽成函数是为了让 `pty` 与 `connector::exec` 用同一条判据与同一句错误
/// 文案——两处各写一遍的话，一处改了另一处没改，用户会看到两种说法。
pub(crate) fn reject_nul(program: &str, args: &[String]) -> Result<(), String> {
    if program.contains('\0') {
        return Err(format!("{program:?}: command name contains a NUL"));
    }
    if let Some(bad) = args.iter().find(|a| a.contains('\0')) {
        return Err(format!("{bad:?}: argument contains a NUL"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(program: &str, args: &[&str]) -> String {
        command_line(
            program,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    /// 普通参数原样穿过：**不多加引号**，报错信息才干净。
    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(
            cmd("afcli.exe", &["run", "--verbose"]),
            "afcli.exe run --verbose"
        );
        assert_eq!(quote("abc"), "abc");
        assert_eq!(quote("a-b_c.d"), "a-b_c.d");
    }

    /// 含空格的参数必须整段加引号，否则会被拆成两个。
    #[test]
    fn an_argument_with_a_space_is_quoted() {
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(
            cmd("C:\\Program Files\\x.exe", &["a b"]),
            "\"C:\\Program Files\\x.exe\" \"a b\""
        );
    }

    /// 空参数也必须引号：不加的话它会在命令行里**整段消失**（变成连续
    /// 两个空格，解析时被跳过）。
    ///
    /// 这条曾经被写成 `!arg.is_empty() && ...` 的前置条件**反掉**——那个
    /// 前置让空参数走进「原样返回」分支，用户传一个空参数会静默丢失。
    #[test]
    fn an_empty_argument_survives_as_two_quotes() {
        assert_eq!(quote(""), "\"\"");
        assert_eq!(cmd("x.exe", &[""]), "x.exe \"\"");
        // 中间的空参数同样要被保住，而不是被两侧空格挤掉。
        assert_eq!(cmd("x.exe", &["a", "", "b"]), "x.exe a \"\" b");
    }

    /// 结尾反斜杠**不**触发加引号（与 Rust `Quote::Auto` 一致）。
    ///
    /// 规则 3：n 个反斜杠后面不跟引号，就是 n 个字面反斜杠。不加引号时
    /// 它们根本不会被当成转义，所以这里原样传过去是正确的。曾经为了让
    /// 「翻倍」那条路被走到而在这里加引号，那是把测试期望写错了。
    #[test]
    fn a_trailing_backslash_alone_does_not_force_quoting() {
        assert_eq!(quote("C:\\dir\\"), "C:\\dir\\");
        assert_eq!(quote("a\\"), "a\\");
        assert_eq!(quote("..\\"), "..\\");
    }

    /// 但**已经要加引号**时，结尾反斜杠必须翻倍。
    ///
    /// 这是最容易漏、后果最重的一条：`"C:\Program Files\x\"` 不加倍的话，
    /// 那个 `\"` 被解析成「一个字面引号」，收尾引号消失，后面的参数全被
    /// 吞进同一个参数里。
    #[test]
    fn a_trailing_backslash_inside_quotes_is_doubled() {
        assert_eq!(quote("C:\\Program Files\\"), "\"C:\\Program Files\\\\\"");
        assert_eq!(quote("a b\\"), "\"a b\\\\\"");
    }

    /// 引号前的反斜杠翻倍，引号本身转义——**且不需要外层引号**。
    ///
    /// `a"b` 直接输出 `a\"b`：解析器看到「0 个反斜杠 + 引号」的奇数形式，
    /// 得到一个字面引号。这对字节与 Rust 一致；多加一对引号虽然也能解析对，
    /// 但字节就不同了。
    #[test]
    fn backslashes_before_a_quote_are_doubled() {
        assert_eq!(quote("a\"b"), "a\\\"b");
        assert_eq!(quote("a\\\"b"), "a\\\\\\\"b");
        // 含空格时既加外层引号、内部也转义。
        assert_eq!(quote("a b\"c"), "\"a b\\\"c\"");
    }

    /// 不含特殊字符的那条路上，反斜杠**不翻倍**（原样返回）。
    #[test]
    fn backslashes_alone_are_left_alone() {
        assert_eq!(quote("C:\\a\\b"), "C:\\a\\b");
    }

    /// 制表符也算分隔符。
    #[test]
    fn a_tab_forces_quoting() {
        assert_eq!(quote("a\tb"), "\"a\tb\"");
    }

    /// `wide` 补一个 NUL，且非 ASCII 走 UTF-16 而不是 UTF-8 字节。
    #[test]
    fn wide_appends_a_nul_and_encodes_utf16() {
        assert_eq!(wide("ab"), vec![0x61, 0x62, 0]);
        assert_eq!(wide("中"), vec![0x4E2D, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    /// 环境块以**两个** NUL 收尾，且追加项覆盖同名键而不是重复出现。
    #[test]
    fn the_environment_block_ends_with_a_double_nul() {
        let block = environment_block(&[("AFCLI_TEST_A".to_string(), "1".to_string())]);
        assert_eq!(block.last(), Some(&0));
        assert_eq!(block.get(block.len() - 2), Some(&0), "要两个 NUL 收尾");
        let text = String::from_utf16_lossy(&block);
        assert_eq!(text.matches("AFCLI_TEST_A=1").count(), 1);
    }

    /// 追加项覆盖同名键：`PATH` 之类被覆盖时**只能出现一次**，否则
    /// `CreateProcessW` 取第一个，用户改了却看不到效果。
    #[test]
    fn an_extra_entry_overrides_the_inherited_one_instead_of_duplicating() {
        let key = "PATH";
        let inherited = std::env::var(key).ok();
        let block = environment_block(&[(key.to_string(), "X:\\override".to_string())]);
        let text = String::from_utf16_lossy(&block);
        let hits: Vec<&str> = text
            .split('\0')
            .filter(|e| e.starts_with(&format!("{key}=")))
            .collect();
        assert_eq!(
            hits,
            vec![format!("{key}=X:\\override")],
            "继承的原值 {inherited:?}"
        );
    }

    /// 含 NUL 的项被跳过，**不截断后面的项**。
    #[test]
    fn an_entry_with_a_nul_is_skipped_and_the_rest_survives() {
        let block = environment_block(&[
            ("GOOD_ONE".to_string(), "a\0b".to_string()),
            ("GOOD_TWO".to_string(), "ok".to_string()),
        ]);
        let text = String::from_utf16_lossy(&block);
        assert!(!text.contains("GOOD_ONE"));
        assert!(text.contains("GOOD_TWO=ok"), "后面的项必须还在：{text:?}");
    }

    /// 键里含 `=` 的项被跳过——否则它会静默改掉**另一个**变量。
    ///
    /// 环境块里 `KEY=VALUE` 的分隔符是第一个 `=`。若不挡，`("A=B", "1")`
    /// 拼出 `A=B=1`，`CreateProcessW` 读成「键 `A`、值 `B=1`」：调用方以为
    /// 设了 `A=B`，实际把 `A` 覆盖了。这条测试钉的就是「宁可不设，也不设错」。
    #[test]
    fn an_entry_whose_key_contains_an_equals_is_skipped_not_silently_rekeyed() {
        let block = environment_block(&[
            ("A=B".to_string(), "1".to_string()),
            ("GOOD_KEY".to_string(), "ok".to_string()),
        ]);
        let text = String::from_utf16_lossy(&block);
        // 关键断言：绝不能出现 `A=B=1` 这种「错键」形态。
        assert!(
            !text.contains("A=B=1"),
            "键含 `=` 的项必须整条跳过：{text:?}"
        );
        assert!(
            !text.split('\0').any(|e| e.starts_with("A=B")),
            "不能留下任何以 `A=B` 开头的项：{text:?}"
        );
        // 同批的其他项不受影响。
        assert!(text.contains("GOOD_KEY=ok"), "后面的项必须还在：{text:?}");
    }

    /// **往返**：把参数拼成命令行，再按 MSVC 规则解析回来，必须一字不差。
    ///
    /// 这条比逐条断言更结实：逐条断言只覆盖到我想到的输入，往返覆盖到
    /// 「解析器怎么读」这个真正的判据。解析器按微软文档的规则手写
    /// （见 `parse_msvc`），它自己也有官方示例的用例守着。
    ///
    /// **这条测试的局限要说清**：`parse_msvc` 与 `quote` 是从同一份规则
    /// 推导出来的两个方向，如果那份规则本身读错了，往返照样全绿。所以它
    /// 挡不住「两边一起错」，只挡「一边单独改错了」。真正的外部判据是下面
    /// 两条：`the_parser_agrees_with_the_documented_examples`（微软原文的
    /// 官方表格）与 `quote_matches_std_quote_auto`（Rust 标准库的实现）。
    #[test]
    fn every_argument_round_trips_through_the_msvc_parser() {
        let cases = [
            "",
            "a",
            "a b",
            "a\tb",
            "C:\\dir\\",
            "C:\\Program Files\\x\\",
            "a\"b",
            "a\\\"b",
            "a b\"c",
            "\\\\\\",
            "\\\"",
            "中 文",
            "%PATH%",
            "--flag=x y",
        ];
        for case in cases {
            let line = quote(case);
            let parsed = parse_msvc(&format!("prog.exe {line}"));
            assert_eq!(
                parsed.get(1).map(String::as_str),
                Some(case),
                "参数 {case:?} 拼成 {line:?} 后解析得到 {parsed:?}"
            );
        }
    }

    /// `quote` 与 Rust 标准库 Windows 侧 `append_arg`（`Quote::Auto`）**逐字节一致**。
    ///
    /// 这是 `quote` 的**外部**判据，不依赖本文件任何一份手写规则：`std` 在
    /// Windows 上拼命令行用的就是 `library/std/src/sys/args/windows.rs` 的
    /// `append_arg`，把同一个参数喂给它，两边必须给出同一串 UTF-16。
    ///
    /// 它为什么能在 Linux 上跑：`append_arg` 里**没有一处系统调用**，就是
    /// 一次 `match`、一次 `any`、一个逐字符循环。这里按它的源码原样复刻
    /// （`std_quote_auto`），并注明出处与版本，改动 std 时这里要跟着对一遍。
    ///
    /// 覆盖的是「判据」这一侧：`Quote::Auto` 只对**空格 / 制表符 / 空串**加
    /// 外层引号，而内部 `"` 永远转义。反例 `a"b` 最能说明问题——它不加外层
    /// 引号（因为没空格），但内部的 `"` 必须写成 `\"`。
    #[test]
    fn quote_matches_std_quote_auto() {
        // 覆盖面刻意做宽：空串、只有空白、只有反斜杠、只有引号、混在一起、
        // 非 ASCII。`\\` 与 `"` 相邻的每一种长度组合都在里面。
        let cases = [
            "",
            " ",
            "\t",
            "a",
            "a b",
            "a\tb",
            " a",
            "a ",
            "\"",
            "a\"b",
            "a\"\"b",
            "\"a b\"",
            "\\",
            "\\\\",
            "\\\"",
            "a\\\"b",
            "C:\\dir\\",
            "C:\\Program Files\\x\\",
            "C:\\Program Files (x86)\\",
            "\\\\server\\share\\",
            "a b\\",
            "a b\"c\\",
            "中 文",
            "中",
            "%PATH%",
            "& | > < ^",
            "--flag=x y",
        ];
        for case in cases {
            assert_eq!(
                quote(case),
                std_quote_auto(case),
                "参数 {case:?} 的引号处理与 std 的 Quote::Auto 不一致"
            );
        }
    }

    /// Rust 标准库 Windows 侧 `append_arg` 在 `Quote::Auto` 下的行为。
    ///
    /// **逐行复刻**自 `library/std/src/sys/args/windows.rs`（`append_arg`，
    /// 判断外层引号的那段 `Quote::Auto => (… , true)`）。复刻而不是调用，
    /// 是因为那个函数是 `std` 私有的、且只在 Windows 上编译。
    ///
    /// 注意这里**故意**把它的判据写成源码里的原样（`' '`、`'\t'`、`is_empty`），
    /// 不要「顺手」改成 `arg.contains([' ', '\t'])` 再和被测实现共用一份——
    /// 共用一份就等于没对过。
    fn std_quote_auto(arg: &str) -> String {
        let bytes = arg.as_bytes();
        let quote = bytes.iter().any(|c| *c == b' ' || *c == b'\t') || bytes.is_empty();
        let escape = true;
        let mut out = String::new();
        if quote {
            out.push('"');
        }
        let mut backslashes: usize = 0;
        for x in arg.chars() {
            if escape {
                if x == '\\' {
                    backslashes += 1;
                } else {
                    if x == '"' {
                        for _ in 0..=backslashes {
                            out.push('\\');
                        }
                    }
                    backslashes = 0;
                }
            }
            out.push(x);
        }
        if quote {
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push('"');
        }
        out
    }

    /// 微软文档给的官方示例表（MSVC「Parsing C++ command-line arguments」）。
    ///
    /// 直接抄文档表格，**不按记忆重写**：这张表是解析器的规范行为，也是
    /// 上一轮我反复写错的地方。用 `r"..."` 原始字面量，`r#"..."#` 里写几个
    /// 反斜杠就是几个——这里要表达的是「命令行里究竟有哪些字符」，不是
    /// Rust 转义语法。
    ///
    /// 表里最容易误读的一行是第 2 行：输入 `a\\b` 用的就是**两个**反斜杠，
    /// 输出也是**两个**反斜杠。因为「反斜杠只在紧跟引号时才特殊」，而这里
    /// `\\` 后面跟的是 `b`，于是原样保留。若把它读成「输入一个、输出一个」，
    /// 这条测试就会红在一个并不存在的 bug 上。
    #[test]
    fn the_parser_agrees_with_the_documented_examples() {
        // `"abc" d e` → abc | d | e
        assert_eq!(
            parse_msvc(r#"prog.exe "abc" d e"#),
            ["prog.exe", "abc", "d", "e"]
        );
        // `a\\b d"e f"g h` → `a\\b` | `de fg` | `h`
        assert_eq!(
            parse_msvc(r#"prog.exe a\\b d"e f"g h"#),
            ["prog.exe", r"a\\b", "de fg", "h"]
        );
        // `a\\\"b c d` → `a\"b` | c | d
        assert_eq!(
            parse_msvc(r#"prog.exe a\\\"b c d"#),
            ["prog.exe", r#"a\"b"#, "c", "d"]
        );
        // `a\\\\"b c" d e` → `a\\b c` | d | e
        assert_eq!(
            parse_msvc(r#"prog.exe a\\\\"b c" d e"#),
            ["prog.exe", r"a\\b c", "d", "e"]
        );
        // `a"b"" c d` → 只有一个参数 `ab" c d`
        assert_eq!(
            parse_msvc(r#"prog.exe a"b"" c d"#),
            ["prog.exe", r#"ab" c d"#]
        );
    }

    /// 按 MSVC 运行时 / `CommandLineToArgvW` 的共同规则解析命令行。
    ///
    /// 只用于测试：生产代码从不解析自己的命令行，这个函数存在的目的就是
    /// 给 `quote` 一个可证的对手。
    fn parse_msvc(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut started = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    // 数连续反斜杠。
                    let mut n = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        n += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        // 2n 个 → n 个反斜杠，引号是分隔符；
                        // 2n+1 个 → n 个反斜杠 + 一个字面引号。
                        for _ in 0..n / 2 {
                            cur.push('\\');
                        }
                        if n % 2 == 1 {
                            cur.push('"');
                            chars.next();
                        }
                        started = true;
                    } else {
                        for _ in 0..n {
                            cur.push('\\');
                        }
                        started = true;
                    }
                }
                '"' => {
                    // 引号内的 `""` 是一个字面引号。
                    if in_quotes && chars.peek() == Some(&'"') {
                        chars.next();
                        cur.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    started = true;
                }
                ' ' | '\t' if !in_quotes => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            }
        }
        if started {
            out.push(cur);
        }
        out
    }

    /// NUL 在命令名或参数里都要在起进程前挡住。
    #[test]
    fn a_nul_in_the_program_or_an_argument_is_refused() {
        assert!(reject_nul("a\0b", &[]).is_err());
        assert!(reject_nul("ok.exe", &["x".to_string(), "a\0b".to_string()]).is_err());
        assert!(reject_nul("ok.exe", &["x".to_string()]).is_ok());
    }
}
