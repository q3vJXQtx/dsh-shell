# -*- coding: utf-8 -*-
"""运行时验证 dsh-shell 通知协议实现（notify.rs 重写后的真实链路验证）。

流程（脚本自始至终自持，不遗留进程）：
  1. 拉起 `dsh --profile web --no-open --port 3099`，从 stdout 抓 ?token=
  2. GET /?token= 换 cookie（禁跟随重定向，期望 303 + Set-Cookie）
  3. 负面用例：不带 cookie 的 WS 升级应被 401 拒绝
  4. 带 cookie + Origin 的 WS 升级 → 期望 101
  5. 发送 open 帧打开 $events 逻辑流 → 期望首帧 value.type == "ready"
  6. POST /api/session/list（带 cookie）→ 期望 result.ok == true
  7. taskkill 收尾
"""
import base64
import json
import os
import re
import socket
import struct
import subprocess
import sys
import time

import requests

PORT = 3099
ORIGIN = f"http://127.0.0.1:{PORT}"
DSH_HOME = os.environ.get("DSH_HOME", r"D:\Software\AI\DSH")
DSH_CMD = os.environ.get("DSH_CMD", r"D:\Software\Code\nodejs\dsh.cmd")
LOG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dsh-server-log.txt")
TOKEN_RE = re.compile(r"token=([A-Za-z0-9_\-]{20,})")

results = []


def report(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name} {detail}")


def wait_for_token(proc, deadline=90.0):
    """读 stdout 直到出现 token，返回 token 字符串。"""
    start = time.time()
    buf = []
    while time.time() - start < deadline:
        line = proc.stdout.readline()
        if not line:
            if proc.poll() is not None:
                break
            continue
        buf.append(line.rstrip())
        m = TOKEN_RE.search(line)
        if m:
            print("--- DSH 启动日志（截取） ---")
            for b in buf[-6:]:
                print("   ", b[:120])
            return m.group(1)
    return None


def exchange_cookie(token):
    s = requests.Session()
    s.trust_env = False
    r = s.get(f"{ORIGIN}/?token={token}", headers={"Origin": ORIGIN},
              allow_redirects=False, timeout=5)
    set_cookie = r.headers.get("set-cookie", "")
    return r.status_code, set_cookie, s


def ws_handshake(cookie=None, path="/api/remote.mux"):
    """裸 socket 做 RFC6455 握手，返回 (socket, 状态行, 响应头)。"""
    sock = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    # ws 库强制 RFC 形状：必须是 16 字节的 base64（22 字符 + "=="）
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: 127.0.0.1:{PORT}\r\n"
        f"Upgrade: websocket\r\n"
        f"Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        f"Sec-WebSocket-Version: 13\r\n"
        f"Origin: {ORIGIN}\r\n"
    )
    if cookie:
        req += f"Cookie: {cookie}\r\n"
    req += "\r\n"
    sock.sendall(req.encode())
    resp = b""
    while b"\r\n\r\n" not in resp:
        chunk = sock.recv(4096)
        if not chunk:
            break
        resp += chunk
    head, _, rest = resp.partition(b"\r\n\r\n")
    return sock, head.decode(errors="replace"), rest


def ws_send_text(sock, text):
    payload = text.encode()
    mask = os.urandom(4)
    n = len(payload)
    if n < 126:
        header = bytes([0x81, 0x80 | n])
    elif n < 65536:
        header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", n)
    else:
        header = bytes([0x81, 0x80 | 127]) + struct.pack(">Q", n)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(header + mask + masked)


def recv_exact(sock, n, deadline=10.0):
    start = time.time()
    data = b""
    while len(data) < n and time.time() - start < deadline:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("socket closed")
        data += chunk
    if len(data) < n:
        raise TimeoutError("recv timeout")
    return data


def recv_text_frame(sock, deadline=10.0):
    """读一个完整 text 帧，跳过 ping/pong/其他。"""
    start = time.time()
    while time.time() - start < deadline:
        b1, b2 = recv_exact(sock, 2, deadline)
        opcode = b1 & 0x0F
        length = b2 & 0x7F
        if length == 126:
            length = struct.unpack(">H", recv_exact(sock, 2, deadline))[0]
        elif length == 127:
            length = struct.unpack(">Q", recv_exact(sock, 8, deadline))[0]
        payload = recv_exact(sock, length, deadline) if length else b""
        if opcode == 0x1:  # text
            return payload.decode()
        # ping(0x9) → 回 pong；其他忽略
        if opcode == 0x9:
            mask = os.urandom(4)
            frame = bytes([0x8A, 0x80 | len(payload)])
            sock.sendall(frame + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))
    raise TimeoutError("no text frame")


def main():
    env = dict(os.environ)
    env["DSH_HOME"] = DSH_HOME
    for k in ("http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env.pop(k, None)
    proc = subprocess.Popen(
        ["cmd", "/c", DSH_CMD, "--profile", "web", "--no-open", "--port", str(PORT)],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, errors="replace", env=env,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    logf = open(LOG_PATH, "w", encoding="utf-8")

    def drain():
        try:
            while True:
                line = proc.stdout.readline()
                if not line:
                    break
                logf.write(line)
                logf.flush()
        except Exception:
            pass

    import threading
    threading.Thread(target=drain, daemon=True).start()

    def wait_for_token(deadline=90.0):
        """轮询日志文件直到出现 token（drain 线程负责写文件）。"""
        start = time.time()
        while time.time() - start < deadline:
            try:
                text = open(LOG_PATH, encoding="utf-8", errors="replace").read()
            except OSError:
                text = ""
            m = TOKEN_RE.search(text)
            if m:
                return m.group(1)
            if proc.poll() is not None:
                return None
            time.sleep(0.2)
        return None

    token = wait_for_token()
    report("DSH 启动并拿到 token", token is not None,
           f"pid={proc.pid}" + (f" token_len={len(token)}" if token else ""))
    if not token:
        logf.close()
        return

    try:
        status, set_cookie, _ = exchange_cookie(token)
        cookie = set_cookie.split(";")[0].strip() if set_cookie else ""
        report("token→cookie 交换（GET / 期望 303 + Set-Cookie）",
               status == 303 and cookie.startswith("dsh-auth-"),
               f"status={status} cookie={cookie[:24]}…")

        # 负面用例：无 cookie 的升级必须被 401 拒绝
        sock_bad, head_bad, _ = ws_handshake(cookie=None)
        report("无 cookie WS 升级被拒（期望 401）", " 401 " in head_bad.splitlines()[0])
        sock_bad.close()

        # 对照实验：假 cookie（形状合法、值无效）——定位 400 来源
        sock_d, head_d, _ = ws_handshake(cookie="foo=bar")
        print("    [对照·假cookie]", head_d.splitlines()[0] if head_d else "(empty)")
        sock_d.close()

        # 正面用例：带 cookie 升级 → 101
        print("    [对照·真cookie] 发送的 Cookie 头长度:", len(cookie))
        sock, head, leftover = ws_handshake(cookie=cookie)
        first_line = head.splitlines()[0] if head else "(empty)"
        print("    [对照·真cookie] 完整响应头:\n", head)
        print("    [对照·真cookie] 响应体:", leftover[:200])
        report("带 cookie WS 升级（期望 101）", " 101 " in first_line, first_line)
        if " 101 " not in first_line:
            return

        # 打开 $events 逻辑流
        open_msg = json.dumps({
            "type": "open", "streamId": "verify-stream-1",
            "endpoint": "$events", "payload": {"args": {}},
        })
        ws_send_text(sock, open_msg)

        # 首帧应为 ready（可能前面有其他控制帧，recv_text_frame 会跳过）
        first_text = recv_text_frame(sock, deadline=15)
        value = json.loads(first_text).get("value", {})
        report("$events 首帧为 ready",
               value.get("type") == "ready" and bool(value.get("clientId")),
               f"clientId={str(value.get('clientId'))[:8]}… host.home={value.get('host', {}).get('home')}")

        # session/list（带 cookie 的一元 RPC）
        s = requests.Session()
        s.trust_env = False
        r = s.post(f"{ORIGIN}/api/session/list",
                   json={"type": "client-request", "rpcId": "verify-1",
                         "method": "session/list",
                         "payload": {"args": {"_request": {}}}},
                   headers={"Origin": ORIGIN, "Cookie": cookie}, timeout=5)
        body = r.json() if r.status_code == 200 else {}
        print("    [session/list] 原始响应:", json.dumps(body, ensure_ascii=False)[:400])
        items = ((body.get("result") or {}).get("value") or {}).get("items") or []
        ok = r.status_code == 200 and (body.get("result") or {}).get("ok") is True and isinstance(items, list)
        sample = [
            {"sessionId": it.get("sessionId"), "running": it.get("running"),
             "title": ((it.get("projections") or {}).get("values") or {}).get("title")}
            for it in items[:3]
        ]
        report("session/list 一元 RPC（期望 ok=true + 数组）", ok,
               f"status={r.status_code} items={len(items)} sample={json.dumps(sample, ensure_ascii=False)}")

        # 收尾：发 close 并断开（Graceful：直接关也行，服务端按代次清理）
        sock.close()
    finally:
        subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                       capture_output=True)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    code = 0
    try:
        main()
    except Exception:
        # 不能吞：finally 里的 sys.exit 会顶替进行中的异常，必须先打印
        import traceback
        traceback.print_exc()
        code = 1
    finally:
        fails = [r for r in results if not r[1]]
        print(f"\n===== 汇总：{len(results) - len(fails)}/{len(results)} 通过 =====")
        sys.exit(code or (1 if fails else 0))
