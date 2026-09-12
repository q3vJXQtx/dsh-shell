//! 访问 token 的获取。
//!
//! # 机制说明（逆向 `@deepseek-ai/dsh-client-connection` 确认）
//!
//! `dsh web` 启动时会向 stdout 打印一行：
//!
//! ```text
//! dsh web: http://127.0.0.1:3080/?token=<43 字符 base64url>
//! ```
//!
//! 这个 token 由 `processLaunchToken()` 在**进程内**生成
//! （`randomBytes(32)` → base64url，故为 43 字符）并存入一个 `WeakMap`，
//! **不落盘**。因此除了抓 stdout，没有别的获取途径。
//!
//! 需要澄清一个容易踩的坑：`.credentials.yaml` 里的
//! `client-connection/browser-session → payload → secret` **不是**这个 token，
//! 两者虽然长度同为 43，但用途不同——实测用 secret 当 token 访问会返回 401。
//!
//! 既然抓取是唯一途径，本模块的目标就是：**尽可能鲁棒，且在失败时能说清原因**。
//! 参考项目在这里只做精确子串匹配，抓不到时静默回退裸 URL，
//! 用户看到 401/404 却完全不知道是"没抓到 token"。

/// token 的字符集（base64url，无填充）
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// 从任意文本中提取 token。
///
/// 相比参考项目的精确匹配 `http://127.0.0.1:3080/?token=`，这里放宽为：
/// - 只锚定 `token=`，不要求整行含完整 URL（容忍 dsh 改变输出前缀）
/// - 大小写不敏感（容忍 `Token=`）
/// - 允许取出后紧跟 `&` / 引号 / 空白等分隔符
///
/// 这样即便 dsh 后续调整输出措辞或加上额外 query 参数，也仍然能抓到。
///
/// **返回最后一个匹配**而非第一个：子进程输出环形缓冲跨代复用时
/// （见 [`crate::logging::clear_child_tail`] 的说明），快照里可能同时
/// 残留上一代的 `token=` 行——launch token 每代随机，旧 token 交换必
/// 401。越靠后的输出越新，因此取末位才是当前这代的 token。
pub fn extract_token(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0usize;
    let mut found: Option<String> = None;
    while let Some(rel) = lower[search_from..].find("token=") {
        let start = search_from + rel + "token=".len();
        let token: String = text[start..]
            .chars()
            .take_while(|c| is_token_char(*c))
            .collect();
        // 43 字符是 32 字节的 base64url；放宽到 20 以上即可接受，
        // 以免 dsh 将来改变随机字节数时抓取失效。
        if token.len() >= 20 {
            found = Some(token);
        }
        search_from = start;
    }
    found
}

/// 从任意文本中提取完整访问 URL（用于日志展示，token 会被打码）
pub fn extract_launch_url(text: &str, port: u16) -> Option<String> {
    let needle = format!("127.0.0.1:{port}");
    for line in text.lines() {
        if !line.contains(&needle) {
            continue;
        }
        // 从行里截出 http... 到空白为止
        if let Some(idx) = line.find("http") {
            let url: String = line[idx..].chars().take_while(|c| !c.is_whitespace()).collect();
            if url.contains("token=") {
                return Some(url);
            }
        }
    }
    None
}

/// 给日志用的打码形式：保留首尾各 4 位，中间遮蔽。
///
/// 目的：日志可能被用户导出并分享求助，不该泄露完整凭证。
pub fn mask_token(token: &str) -> String {
    let n = token.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = token.chars().take(4).collect();
    let tail: String = token.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// 构造带 token 的访问 URL
pub fn build_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/?token={token}")
}

/// 把 URL 里的 token **打码**，用于写日志。
///
/// 日志会被导出成诊断报告发给别人排障，所以任何可能落盘的 URL
/// 都必须先过这里 —— 完整 token 等于一把可直接访问本机 DSH 的钥匙。
///
/// 与 [`extract_token`] 的区别：这里**不做长度门限**，
/// 无论 token 长短一律打码（宁可多打码，也不能漏一个）。
pub fn mask_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let mut out = String::with_capacity(url.len());
    let mut cursor = 0usize;

    while let Some(rel) = lower[cursor..].find("token=") {
        let val_start = cursor + rel + "token=".len();
        let val_len: usize = url[val_start..]
            .chars()
            .take_while(|c| is_token_char(*c))
            .map(|c| c.len_utf8())
            .sum();
        let val_end = val_start + val_len;

        out.push_str(&url[cursor..val_start]);
        out.push_str(&mask_token(&url[val_start..val_end]));
        cursor = val_end;
    }

    out.push_str(&url[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 token 的形状：`randomBytes(32)` 经**无填充** base64url 编码 = **43 字符**。
    ///
    /// 依据 `@deepseek-ai/dsh-client-connection`：`SECRET_BYTES = 32`，
    /// `encodeBase64Url()` 做 `+`→`-`、`/`→`_` 并剥掉 `=` 填充。
    /// 32 字节 → 标准 base64 为 44 字符（含 1 个 `=`）→ 去掉填充即 43。
    ///
    /// 下面这个样本刻意同时含 `-` 和 `_`，用来覆盖 base64url 的字符集替换
    /// （早期版本曾因只认 `[A-Za-z0-9]` 而在真实 token 上截断）。
    const REAL_TOKEN: &str = "4OHi4-Tl5ufo6err7O3u7_Dx8vP09fb3-Pn6-_z9_v8";

    const REAL_LINE: &str =
        "dsh web: http://127.0.0.1:3080/?token=4OHi4-Tl5ufo6err7O3u7_Dx8vP09fb3-Pn6-_z9_v8";

    #[test]
    fn real_token_shape_is_43_chars() {
        // 守住"43 字符"这个事实本身，避免将来又被改错
        assert_eq!(REAL_TOKEN.len(), 43);
        assert!(extract_token(REAL_LINE).is_some(), "样本行必须能被解析");
    }

    #[test]
    fn extracts_from_real_output() {
        let t = extract_token(REAL_LINE).expect("应能抓到 token");
        assert_eq!(t, REAL_TOKEN);
        assert_eq!(t.len(), 43);
        // 完整取出，没有被 base64url 特殊字符提前截断
        assert!(t.contains('-') && t.contains('_'), "实际: {t}");
    }

    #[test]
    fn tolerant_to_prefix_changes() {
        // dsh 换了输出前缀，仍应抓到
        let line = "INFO server ready -> http://127.0.0.1:3080/?token=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(extract_token(line).is_some());
        // 甚至完全没有 http 前缀
        let bare = "token=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB done";
        assert_eq!(extract_token(bare).unwrap().len(), 43);
    }

    #[test]
    fn case_insensitive() {
        let line = "URL: http://x/?Token=CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
        assert!(extract_token(line).is_some());
    }

    #[test]
    fn stops_at_separator() {
        let line = "http://127.0.0.1:3080/?token=DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD&profile=web";
        let t = extract_token(line).unwrap();
        assert_eq!(t.len(), 43);
        assert!(!t.contains('&'));
    }

    #[test]
    fn ignores_too_short() {
        assert!(extract_token("token=abc").is_none());
    }

    #[test]
    fn prefers_the_last_token_in_buffer() {
        // 回归：环形缓冲跨代复用时可能残留上一代的 token= 行。
        // 事件流连不上的根因就是取了第一个（最老的）——旧 token 交换必 401。
        let stale = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let fresh = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let text = format!(
            "dsh web: http://127.0.0.1:3099/?token={stale}\n\
             正在启动…\n\
             dsh web: http://127.0.0.1:3099/?token={fresh}"
        );
        assert_eq!(extract_token(&text).unwrap(), fresh, "必须取最新一代的 token");
    }

    #[test]
    fn no_token_returns_none() {
        assert!(extract_token("dsh web: listening on port 3080").is_none());
    }

    #[test]
    fn extracts_token_from_json_quoted_field() {
        // 有些输出会把 URL 放进引号里，引号不能混进 token
        let line = format!(r#"{{"url":"http://127.0.0.1:3080/?token={REAL_TOKEN}"}}"#);
        assert_eq!(extract_token(&line).unwrap(), REAL_TOKEN);
    }

    #[test]
    fn extracts_launch_url_for_display() {
        let url = extract_launch_url(REAL_LINE, 3080).expect("应能截出 URL");
        assert_eq!(url, format!("http://127.0.0.1:3080/?token={REAL_TOKEN}"));
        // 端口不匹配时不认
        assert!(extract_launch_url(REAL_LINE, 9999).is_none());
    }

    #[test]
    fn mask_hides_middle() {
        let m = mask_token(REAL_TOKEN);
        assert_eq!(m, "4OHi…9_v8");
        assert!(!m.contains("4-Tl5ufo6err"), "中段必须被遮蔽");
    }

    #[test]
    fn mask_handles_short() {
        assert_eq!(mask_token("abc"), "***");
    }

    #[test]
    fn builds_url() {
        assert_eq!(build_url(3080, "T"), "http://127.0.0.1:3080/?token=T");
    }

    #[test]
    fn mask_url_hides_token_but_keeps_rest() {
        let url = format!("http://127.0.0.1:3099/?token={REAL_TOKEN}");
        assert_eq!(
            mask_url(&url),
            "http://127.0.0.1:3099/?token=4OHi…9_v8",
            "URL 其余部分要保持原样，便于排障时确认端口/路径"
        );
        // 完整 token 绝不能出现在结果里
        assert!(!mask_url(&url).contains(REAL_TOKEN));
    }

    #[test]
    fn mask_url_handles_no_token_and_extra_params() {
        // 没有 token 的原样返回
        assert_eq!(mask_url("http://127.0.0.1:3080/"), "http://127.0.0.1:3080/");
        // token 后面还有别的查询参数时，不能把后面的参数一起吃掉
        let with_extra = format!("http://x/?token={REAL_TOKEN}&profile=web");
        assert_eq!(
            mask_url(&with_extra),
            "http://x/?token=4OHi…9_v8&profile=web"
        );
    }

    #[test]
    fn mask_url_masks_short_token_too() {
        // 不做长度门限：短 token 也一律打码
        assert_eq!(mask_url("http://x/?token=abc"), "http://x/?token=***");
    }
}
