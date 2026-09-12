//! 注入到 DSH 页面里的两段脚本：链接行为 + GitHub 加速。
//!
//! # 为什么是注入脚本，而不是改 DSH 的页面
//!
//! DSH 的 webchat 是它自己的前端产物，我们既不该也不能去改它的源码。
//! 但 WebView2 支持在**每个文档创建时**先跑一段我们的脚本，
//! 这给了我们一个合法的"旁路"：页面自己不知道，用户却能获得更好的体验。
//!
//! # 为什么不需要 origin 守卫（与参考项目的关键差异）
//!
//! 参考项目把 webchat 加载进壳页面的 iframe / 或直接导航主窗口过去，
//! 所以脚本会在**每个页面**都执行，必须靠
//! `location.origin === 'http://127.0.0.1:3080'` 自保。
//!
//! 本项目里 webchat 是一个**独立的子 WebView**（见 [`crate::chat`]），
//! 脚本只注入给它，压根不会跑到 React 控制台上——守卫因此是多余的。
//!
//! 顺带修掉了参考项目的一个隐藏问题：那个硬编码的 `3080` 守卫在
//! **用户改了端口之后会静默失效**，右键菜单和外链接管一起失灵，
//! 而且不报任何错。本项目端口可配置，所以这里绝不能依赖硬编码端口。
//!
//! # 两段脚本的分工
//!
//! - [`LINK_SCRIPT`] —— 把 WebView2 那个通用的 Edge 右键菜单，
//!   在链接上换成两项真正有用的操作；顺带把外链点击导向系统浏览器。
//! - [`GH_MIRROR_SCRIPT`] —— 把发往 GitHub 的请求改走加速镜像。
//!   插件常要拉 GitHub 上的文件，国内直连大概率失败。

/// 链接行为脚本。
///
/// 干两件事：
///
/// 1. **右键**：链接上弹出自建菜单（在浏览器中打开 / 复制链接）。
///    非链接的右键保持 WebView2 默认菜单——用户可能要用「刷新」「检查」。
/// 2. **左键**：接管 `target="_blank"` 与 Ctrl/Shift+点击的外链。
///
/// 第 2 点为什么必须做：`tauri-plugin-opener` 会注入一段脚本，在**冒泡阶段**
/// 抢走这类点击并改走页面内的 Tauri IPC——而 DSH 页面跑在
/// `http://127.0.0.1:3080`，属于远程上下文，IPC 桥不可靠。结果是
/// 原生新窗口被 `preventDefault()` 压掉、IPC 又调不通，**两头落空**：
/// 用户点外链毫无反应（参考项目 v2.0.2 修的就是这个）。
///
/// 这里用**捕获阶段**处理器抢在它前面接管，`preventDefault()` 之后改走
/// `window.open` —— 与右键菜单同一条 `on_new_window → 系统浏览器` 通道。
/// 另外项目侧也已关闭 opener 的 JS 拦截（见 `lib.rs`），两道保险。
pub const LINK_SCRIPT: &str = r#"
(function () {
  if (window.__dshShellLink) return;
  window.__dshShellLink = true;

  var menu = null;
  function closeMenu() {
    if (menu) {
      if (menu.parentNode) menu.parentNode.removeChild(menu);
      menu = null;
    }
  }

  function openInBrowser(url) {
    // 走原生新窗口请求；壳把它交给系统默认浏览器
    window.open(url, '_blank', 'noopener');
  }

  function copyLink(url, row) {
    var done = function () {
      if (!row) return;
      row.textContent = '已复制';
      setTimeout(closeMenu, 500);
    };
    var fallback = function () {
      // navigator.clipboard 在非安全上下文里不存在（http://127.0.0.1 算安全的，
      // 但用户换了自定义域名 / 端口转发后就未必），所以留一条老路。
      try {
        var ta = document.createElement('textarea');
        ta.value = url;
        ta.setAttribute('style', 'position:fixed;opacity:0;');
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        done();
      } catch (e) {
        if (row) row.textContent = '复制失败';
      }
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(url).then(done, fallback);
    } else {
      fallback();
    }
  }

  function showMenu(x, y, url) {
    closeMenu();
    menu = document.createElement('div');
    menu.setAttribute('style',
      'position:fixed;z-index:2147483647;min-width:170px;padding:4px 0;' +
      'background:#1c1f26;border:1px solid #3a4050;border-radius:8px;' +
      'box-shadow:0 8px 24px rgba(0,0,0,.45);font:13px/1 system-ui,sans-serif;' +
      'color:#e6e6e6;-webkit-user-select:none;user-select:none;'
    );

    var items = [
      ['在浏览器中打开', function () { openInBrowser(url); }],
      ['复制链接', function (row) { copyLink(url, row); }]
    ];

    items.forEach(function (item) {
      var row = document.createElement('div');
      row.textContent = item[0];
      row.setAttribute('style', 'padding:7px 14px;cursor:pointer;white-space:nowrap;');
      row.addEventListener('mouseenter', function () { row.style.background = '#2a3040'; });
      row.addEventListener('mouseleave', function () { row.style.background = 'transparent'; });
      row.addEventListener('click', function (e) {
        e.stopPropagation();
        // 「复制链接」要保留菜单以便回显"已复制"，其余点击后立刻收起
        item[1](row);
        if (item[0] !== '复制链接') closeMenu();
      });
      menu.appendChild(row);
    });

    document.documentElement.appendChild(menu);
    var w = menu.offsetWidth, h = menu.offsetHeight;
    menu.style.left = Math.min(x, innerWidth - w - 8) + 'px';
    menu.style.top = Math.min(y, innerHeight - h - 8) + 'px';
  }

  // 左键 / Ctrl+点击外链 —— 捕获阶段，抢在 opener 的注入脚本之前
  document.addEventListener('click', function (e) {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.altKey) return;
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a || !a.href) return;
    var isBlank = (a.target || '').toLowerCase() === '_blank';
    var isModifier = e.ctrlKey || e.shiftKey;
    if (!isBlank && !isModifier) return;
    var u;
    try { u = new URL(a.href); } catch (err) { return; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return;
    // 站内链接交还给页面自己处理：DSH 前端有自己的路由
    if (u.host === location.host) return;
    e.preventDefault();
    e.stopPropagation();
    openInBrowser(a.href);
  }, true);

  document.addEventListener('contextmenu', function (e) {
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a) return;
    e.preventDefault();
    e.stopPropagation();
    showMenu(e.clientX, e.clientY, a.href);
  }, true);

  document.addEventListener('mousedown', function (e) {
    if (menu && !menu.contains(e.target)) closeMenu();
  }, true);
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') closeMenu();
  }, true);
  document.addEventListener('scroll', closeMenu, true);
  window.addEventListener('blur', closeMenu);
  window.addEventListener('resize', closeMenu);
})();
"#;

/// GitHub 镜像脚本。
///
/// DSH 的插件在安装/更新时会从 GitHub 拉文件，国内直连基本不通。
/// 这里在页面层面把 `fetch` / `XMLHttpRequest` 的目标 URL 重写到
/// [gh-proxy](https://gh-proxy.com) 镜像前缀。
///
/// **只影响 GitHub 域名**：`127.0.0.1` 与其它站点一律原样放行。
/// 用户可以在设置里关掉它（有些网络环境直连 GitHub 更快，
/// 而走第三方镜像既多一跳、也把请求地址暴露给了镜像服务）。
pub const GH_MIRROR_SCRIPT: &str = r#"
(function () {
  if (window.__dshShellGhMirror) return;
  window.__dshShellGhMirror = true;

  var MIRROR = 'https://gh-proxy.com/';
  // 刻意**不含 `api.github.com`**。
  //
  // 那上面的请求经常带 `Authorization: Bearer <token>`，而镜像是个
  // **第三方服务** —— 把它加入列表就等于把用户的 GitHub 凭据交给对方。
  // releases / raw / codeload / objects 这些下载路径靠公开地址或
  // 签名查询串即可访问，不需要认证头，走镜像才是安全的。
  var PREFIXES = [
    'https://github.com/',
    'https://raw.githubusercontent.com/',
    'https://gist.githubusercontent.com/',
    'https://codeload.github.com/',
    'https://objects.githubusercontent.com/',
    'https://github-releases.githubusercontent.com/'
  ];

  function mirrorize(u) {
    if (typeof u !== 'string' || u.indexOf('://') < 0) return u;
    // 已经是镜像地址就不再套一层（否则会变成 gh-proxy.com/https://gh-proxy.com/…）
    if (u.indexOf(MIRROR) === 0) return u;
    for (var i = 0; i < PREFIXES.length; i++) {
      if (u.indexOf(PREFIXES[i]) === 0) {
        var full = MIRROR + u;
        console.log('[DSH Shell] GitHub 请求已走加速镜像:', u, '->', full);
        return full;
      }
    }
    return u;
  }

  try {
    var originalFetch = window.fetch ? window.fetch.bind(window) : null;
    if (originalFetch) {
      window.fetch = function (input, init) {
        // 整段重写都包在 try 里。
        //
        // `new Request(url, request)` 在**同一个 Request 的 body 已被消费**时
        // 会**同步抛异常**（页面重试同一个 Request 对象就会碰上）。
        // 而 fetch 的契约是"永远返回 Promise、绝不同步抛" ——
        // 一旦这里同步抛出，调用方写的 `fetch(...).catch(...)` 根本接不到，
        // 会变成未捕获异常。所以出错就原样放行：宁可这一跳不过镜像，
        // 也不能把页面弄崩。
        try {
          if (typeof input === 'string') {
            input = mirrorize(input);
          } else if (input && input.url) {
            // Request 对象本身可作为 init 传回（method/headers/body 都在里面）
            input = new Request(mirrorize(input.url), input);
          }
        } catch (e) {
          console.warn('[DSH Shell] 镜像重写失败，已按原地址请求:', e);
        }
        return originalFetch(input, init);
      };
    }
  } catch (e) { /* 保底：镜像失败也绝不能让页面挂掉 */ }

  try {
    var originalOpen = XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open = function (method, url) {
      arguments[1] = mirrorize(String(url));
      return originalOpen.apply(this, arguments);
    };
  } catch (e) { /* 同上 */ }
})();
"#;

/// 组装要注入的全部脚本。
///
/// `gh_mirror` 为假时只注入链接脚本——开关在设置里。
pub fn scripts(gh_mirror: bool) -> String {
    if gh_mirror {
        format!("{LINK_SCRIPT}\n{GH_MIRROR_SCRIPT}")
    } else {
        LINK_SCRIPT.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_script_has_no_hardcoded_port() {
        // 端口可配置，脚本里出现 3080 就意味着换端口后功能会静默失效
        assert!(
            !LINK_SCRIPT.contains("3080"),
            "链接脚本不应硬编码端口（参考项目正是因此踩坑）"
        );
    }

    #[test]
    fn gh_mirror_only_targets_github() {
        assert!(!GH_MIRROR_SCRIPT.contains("127.0.0.1"));
        assert!(GH_MIRROR_SCRIPT.contains("github.com"));
    }

    /// 镜像**绝不能**代理 GitHub 的 API 域名。
    ///
    /// `api.github.com` 上的请求经常带 `Authorization: Bearer <token>`，
    /// 而镜像是第三方服务 —— 加进列表就等于把用户凭据交给它。
    /// 下载路径（releases / raw / codeload）不需要认证，走镜像才安全。
    #[test]
    fn gh_mirror_never_proxies_the_authenticated_github_api() {
        // 只检查真正的镜像列表。不能整篇扫字符串 ——
        // 脚本的注释里会提到 api.github.com（说明为什么不代理它），
        // 那样会把解释性文字误判成配置。
        let start = GH_MIRROR_SCRIPT
            .find("var PREFIXES = [")
            .expect("脚本里应当有镜像前缀列表");
        let rest = &GH_MIRROR_SCRIPT[start..];
        let end = rest.find("];").expect("前缀列表应当以 ]; 收尾");
        let list = &rest[..end];

        assert!(
            !list.contains("api.github.com"),
            "镜像列表里出现了 api.github.com —— 那里可能带认证头，不应经过第三方镜像"
        );
        // 但下载路径必须留着，否则镜像功能就形同虚设
        assert!(
            list.contains("raw.githubusercontent.com"),
            "下载路径应当保留在镜像列表里"
        );
        assert!(list.contains("codeload.github.com"));
    }

    /// fetch 包装不能**同步**抛异常。
    ///
    /// `new Request(url, request)` 在同一个 Request 的 body 已被消费时会同步抛，
    /// 而 fetch 的契约是"永远返回 Promise"。一旦同步抛出，页面里写的
    /// `fetch(...).catch(...)` 接不到，会变成未捕获异常。
    #[test]
    fn fetch_wrapper_never_throws_synchronously() {
        let start = GH_MIRROR_SCRIPT
            .find("window.fetch = function")
            .expect("脚本里应当有 fetch 包装");
        let body = &GH_MIRROR_SCRIPT[start..];
        let end = body
            .find("return originalFetch")
            .expect("fetch 包装应当最终回退到原始实现");
        let rewrite = &body[..end];

        assert!(
            rewrite.contains("try {") && rewrite.contains("catch (e)"),
            "URL 重写过程必须包在 try/catch 里，否则构造 Request 失败会同步抛出"
        );
    }

    #[test]
    fn scripts_respects_the_toggle() {
        let off = scripts(false);
        assert!(!off.contains("gh-proxy"));
        assert!(off.contains("__dshShellLink"));

        let on = scripts(true);
        assert!(on.contains("gh-proxy"));
        assert!(on.contains("__dshShellGhMirror"));
    }

    #[test]
    fn scripts_are_self_guarding() {
        // 注入脚本会随每次导航重跑，必须自带幂等守卫
        assert!(LINK_SCRIPT.contains("__dshShellLink"));
        assert!(LINK_SCRIPT.contains("if (window.__dshShellLink) return"));
    }
}
