//! 就绪探测 —— 区分"有东西在应答"与"DSH 真的在跑"。
//!
//! # 为什么不能只看状态码
//!
//! 参考项目的探测逻辑是：
//!
//! ```text
//! GET http://127.0.0.1:3080/
//!   → 2xx/3xx          => 就绪
//!   → 401/403/404      => 也就绪（注释："认证墙也证明服务在跑"）
//!   → 连接失败/超时    => 未就绪
//! ```
//!
//! 放宽到这个程度是为了兼容 dsh-remote 的登录墙，但副作用很大：
//! **任何**占用 3080 的东西都会被当成 DSH。实际排查中就撞见过——
//! 本机环境变量代理会把访问 127.0.0.1 的请求吃掉并返回
//! `502 Bad Gateway / upstream connect failed`，这在原逻辑下同样算"就绪"。
//!
//! 本模块把判断拆成两层：
//! - [`ProbeResult::Responding`]：端口有响应（用于"等待就绪"的轮询）
//! - [`ServiceKind`]：这个响应**到底是什么**（用于决策与诊断）

use std::time::Duration;

/// 服务识别结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceKind {
    /// 确实是 DSH，且当前请求已通过认证
    DshAuthenticated,
    /// 确实是 DSH，但需要 token（这是**正常**的未认证态，不是故障）
    DshNeedsToken,
    /// 像是 DSH 的其它非 2xx 状态
    DshOther(u16),
    /// 是反向代理的错误页，**不是** DSH
    ProxyError { detail: String },
    /// 其它未知服务
    UnknownService { status: u16, body_head: String },
}

impl ServiceKind {
    /// 是否可判定为"DSH 已就绪"
    pub fn is_dsh(&self) -> bool {
        matches!(
            self,
            ServiceKind::DshAuthenticated
                | ServiceKind::DshNeedsToken
                | ServiceKind::DshOther(_)
        )
    }

    /// 面向用户的描述（用于诊断面板）
    pub fn describe(&self) -> String {
        match self {
            ServiceKind::DshAuthenticated => "DSH 已就绪且已验证".into(),
            ServiceKind::DshNeedsToken => "DSH 已就绪（等待访问凭证）".into(),
            ServiceKind::DshOther(code) => format!("DSH 返回状态码 {code}"),
            ServiceKind::ProxyError { detail } => {
                format!("端口上的服务像一个反向代理而非 DSH：{detail}")
            }
            ServiceKind::UnknownService { status, .. } => {
                format!("端口被其它服务占用（HTTP {status}）")
            }
        }
    }
}

/// 一次探测的结果
#[derive(Debug, Clone)]
pub enum ProbeResult {
    /// 连接被拒绝：没有东西在监听
    Refused,
    /// 超时
    Timeout,
    /// 有 HTTP 响应，并已识别
    Responding(ServiceKind),
    /// 其它错误
    Error(String),
}

impl ProbeResult {
    /// 端口上是否有任何 HTTP 响应（宽松判断，用于等待循环）
    pub fn has_response(&self) -> bool {
        matches!(self, ProbeResult::Responding(_))
    }
}

/// 探测目标端口。
///
/// `token` 传入时会带上它，从而能区分"已认证"与"需要 token"。
pub fn probe(port: u16, token: Option<&str>) -> ProbeResult {
    let url = match token {
        Some(t) => format!("http://127.0.0.1:{port}/?token={t}"),
        None => format!("http://127.0.0.1:{port}/"),
    };
    // 关键：显式禁用代理。本机存在 http_proxy 环境变量，
    // 若不绕过，访问 127.0.0.1 会走代理并收到假的 502。
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3))
        .try_proxy_from_env(false)
        .build();

    match agent.get(&url).call() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.into_string().unwrap_or_default();
            ProbeResult::Responding(classify(status, &body))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            ProbeResult::Responding(classify(status, &body))
        }
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            if msg.contains("refused") || msg.contains("10061") {
                ProbeResult::Refused
            } else if msg.contains("timed out") || msg.contains("timeout") {
                ProbeResult::Timeout
            } else {
                ProbeResult::Error(msg)
            }
        }
    }
}

/// 根据状态码与响应体识别服务类型。
///
/// 识别代理错误是本函数的关键价值：本机实测代理会返回
/// `502` 且响应体为 `upstream connect failed: ...`。
fn classify(status: u16, body: &str) -> ServiceKind {
    let head: String = body.chars().take(200).collect();

    // 优先排除"像代理错误页"的响应——它常常是 502/503/504
    if (500..=599).contains(&status) {
        let lower = head.to_ascii_lowercase();
        let looks_like_proxy = lower.contains("upstream")
            || lower.contains("proxy")
            || lower.contains("bad gateway")
            || lower.contains("gateway");
        if looks_like_proxy {
            return ServiceKind::ProxyError {
                detail: head.chars().take(80).collect(),
            };
        }
    }

    match status {
        // 303 是 DSH 带 token 访问成功后的重定向（实测行为）
        200..=399 => ServiceKind::DshAuthenticated,
        // 401 是 DSH 的标准"需要 token"响应（实测无 token 访问得到 401）
        401 => ServiceKind::DshNeedsToken,
        // 404 也可能来自 DSH（参考项目注释与实测都出现过）
        404 => ServiceKind::DshOther(404),
        _ => ServiceKind::UnknownService { status, body_head: head },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauth_is_dsh_needs_token() {
        let k = classify(401, "Unauthorized");
        assert_eq!(k, ServiceKind::DshNeedsToken);
        assert!(k.is_dsh());
    }

    #[test]
    fn redirect_is_authenticated() {
        assert_eq!(classify(303, ""), ServiceKind::DshAuthenticated);
        assert_eq!(classify(200, "<html>"), ServiceKind::DshAuthenticated);
    }

    #[test]
    fn proxy_502_is_not_dsh() {
        // 真实抓到的代理响应体
        let k = classify(
            502,
            "upstream connect failed: 由于目标计算机积极拒绝，无法连接。 (os error 10061)",
        );
        match k {
            ServiceKind::ProxyError { .. } => {}
            other => panic!("应识别为代理错误，实际 {other:?}"),
        }
        assert!(!classify(502, "upstream connect failed").is_dsh());
    }

    #[test]
    fn unknown_service_detected() {
        let k = classify(418, "I'm a teapot");
        match k {
            ServiceKind::UnknownService { status, .. } => assert_eq!(status, 418),
            other => panic!("应识别为未知服务，实际 {other:?}"),
        }
        assert!(!k.is_dsh());
    }

    #[test]
    fn probe_result_has_response() {
        assert!(ProbeResult::Responding(ServiceKind::DshNeedsToken).has_response());
        assert!(!ProbeResult::Refused.has_response());
        assert!(!ProbeResult::Timeout.has_response());
    }

    #[test]
    fn descriptions_are_non_empty() {
        assert!(!ServiceKind::DshNeedsToken.describe().is_empty());
        assert!(!ServiceKind::ProxyError { detail: "x".into() }
            .describe()
            .is_empty());
    }
}
