"""生成 DSH Shell 的应用图标源图（1024x1024 PNG）。

用 4 倍超采样再缩小，以获得平滑的边缘（PIL 本身不做抗锯齿）。
图形语义：终端提示符 ">_" —— 对应本项目"把命令行服务装进桌面窗口"的定位。
"""

import os

from PIL import Image, ImageDraw

S = 4096  # 超采样尺寸
OUT = 1024  # 最终尺寸

# ---- 背景：圆角矩形 + 垂直渐变 ----
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

top = (86, 148, 255)
bottom = (37, 99, 235)
grad = Image.new("RGBA", (S, S))
gd = ImageDraw.Draw(grad)
for y in range(S):
    t = y / (S - 1)
    color = tuple(int(top[i] * (1 - t) + bottom[i] * t) for i in range(3))
    gd.line([(0, y), (S, y)], fill=color + (255,))

mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle(
    [0, 0, S - 1, S - 1], radius=int(S * 0.22), fill=255
)
img.paste(grad, (0, 0), mask)

# ---- 前景：">_" ----
d = ImageDraw.Draw(img)
stroke = int(S * 0.070)
r = stroke // 2
white = (255, 255, 255, 255)

# 折线 ">"
p_start = (int(S * 0.30), int(S * 0.33))
p_mid = (int(S * 0.50), int(S * 0.50))
p_end = (int(S * 0.30), int(S * 0.67))
d.line([p_start, p_mid], fill=white, width=stroke)
d.line([p_mid, p_end], fill=white, width=stroke)

# 下划线 "_"
u_start = (int(S * 0.58), int(S * 0.67))
u_end = (int(S * 0.75), int(S * 0.67))
d.line([u_start, u_end], fill=white, width=stroke)

# 端点补圆，模拟圆头线帽
for cx, cy in (p_start, p_mid, p_end, u_start, u_end):
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=white)

OUT_FILE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), os.pardir, "app-icon.png"
)
img.resize((OUT, OUT), Image.LANCZOS).save(OUT_FILE)
print(f"icon written -> {OUT_FILE}")
