"""截图工具：裁剪、比对、找位移、叠坐标网格、拼图、取色。

所有坐标都是 Aero 窗口里的点（左上角原点），与 `aero-ui click` 用的坐标一致。
截图保存成 PNG 时在文本块里记下缩放（aero-scale）和裁剪原点（aero-origin），
所以裁剪过的图也按窗口坐标报告。由 `aero-ui` 调用，也可以单独用：

    img.py tag   <png> <窗口宽> [x y w h]        记下缩放，按窗口坐标裁剪（原地）
    img.py crop  <src> <dst> x y w h
    img.py diff  <a> <b> [out] [--ignore x y w h]...
    img.py shift <a> <b> x y w h [--range 6]
    img.py grid  <src> <dst> [步长]
    img.py stack <dst> <png>...
    img.py color <png> x y
"""

import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont
from PIL.PngImagePlugin import PngInfo


class Shot:
    def __init__(self, path):
        im = Image.open(path)
        self.scale = float(im.info.get("aero-scale", 2))
        self.ox, self.oy = (float(v) for v in im.info.get("aero-origin", "0 0").split())
        self.im = im.convert("RGB")

    def px(self, x, y):
        """窗口点 → 本图像素。"""
        return round((x - self.ox) * self.scale), round((y - self.oy) * self.scale)

    def pt(self, px, py):
        """本图像素 → 窗口点。"""
        return px / self.scale + self.ox, py / self.scale + self.oy

    def crop(self, x, y, w, h):
        x0, y0 = self.px(x, y)
        x1, y1 = self.px(x + w, y + h)
        out = Shot.__new__(Shot)
        out.scale, out.ox, out.oy = self.scale, float(x), float(y)
        out.im = self.im.crop((x0, y0, x1, y1))
        return out

    def save(self, path):
        info = PngInfo()
        info.add_text("aero-scale", f"{self.scale:g}")
        info.add_text("aero-origin", f"{self.ox:g} {self.oy:g}")
        self.im.save(path, pnginfo=info)


def nums(args, n):
    if len(args) < n:
        sys.exit(f"需要 {n} 个数字，得到 {args}")
    return [float(v) for v in args[:n]]


def font(size):
    for name in ("/System/Library/Fonts/Supplemental/Arial.ttf", "/System/Library/Fonts/Helvetica.ttc"):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            pass
    return ImageFont.load_default()


def runs(indices, gap):
    """把排好序的下标切成相邻间距不超过 gap 的段。"""
    out = []
    for i in indices:
        if out and i - out[-1][1] <= gap:
            out[-1][1] = i
        else:
            out.append([i, i])
    return out


def cmd_tag(args):
    path = args[0]
    shot = Shot(path)
    win_w = float(args[1])
    shot.scale = shot.im.width / win_w
    shot.ox = shot.oy = 0.0
    if len(args) >= 6:
        shot = shot.crop(*nums(args[2:], 4))
    shot.save(path)


def cmd_crop(args):
    Shot(args[0]).crop(*nums(args[2:], 4)).save(args[1])


def cmd_diff(args):
    ignores = []
    rest = []
    i = 0
    while i < len(args):
        if args[i] == "--ignore":
            ignores.append(nums(args[i + 1 :], 4))
            i += 5
        else:
            rest.append(args[i])
            i += 1
    a, b = Shot(rest[0]), Shot(rest[1])
    if a.im.size != b.im.size:
        print(f"尺寸不同：{a.im.size} vs {b.im.size}，只比较重叠部分")
    w, h = min(a.im.width, b.im.width), min(a.im.height, b.im.height)
    pa = np.asarray(a.im, dtype=np.int16)[:h, :w]
    pb = np.asarray(b.im, dtype=np.int16)[:h, :w]
    mask = np.abs(pa - pb).sum(axis=2) > 40
    for x, y, iw, ih in ignores:
        x0, y0 = a.px(x, y)
        x1, y1 = a.px(x + iw, y + ih)
        mask[max(y0, 0) : max(y1, 0), max(x0, 0) : max(x1, 0)] = False
    gap = int(3 * a.scale)
    boxes = []
    for r0, r1 in runs(np.flatnonzero(mask.any(axis=1)), gap):
        band = mask[r0 : r1 + 1]
        for c0, c1 in runs(np.flatnonzero(band.any(axis=0)), gap):
            boxes.append((c0, r0, c1 + 1, r1 + 1, int(band[:, c0 : c1 + 1].sum())))
    if not boxes:
        print("相同")
    else:
        print(f"{len(boxes)} 处差异（窗口点）：")
        for c0, r0, c1, r1, count in boxes:
            x0, y0 = a.pt(c0, r0)
            x1, y1 = a.pt(c1, r1)
            print(f"  x {x0:.1f}–{x1:.1f}  y {y0:.1f}–{y1:.1f}  ({x1 - x0:.1f}×{y1 - y0:.1f}pt，{count} 像素)")
    if len(rest) > 2:
        faded = (pb * 0.3 + 255 * 0.7).astype(np.uint8)
        faded[mask] = (255, 0, 0)
        out = Image.fromarray(faded)
        draw = ImageDraw.Draw(out)
        pad = int(2 * a.scale)
        for c0, r0, c1, r1, _ in boxes:
            draw.rectangle((c0 - pad, r0 - pad, c1 + pad, r1 + pad), outline=(255, 0, 0), width=max(1, int(a.scale)))
        shot = Shot.__new__(Shot)
        shot.scale, shot.ox, shot.oy, shot.im = a.scale, a.ox, a.oy, out
        shot.save(rest[2])


def cmd_shift(args):
    reach = 6.0
    if "--range" in args:
        k = args.index("--range")
        reach = float(args[k + 1])
        args = args[:k] + args[k + 2 :]
    a, b = Shot(args[0]), Shot(args[1])
    x, y, w, h = nums(args[2:], 4)
    ga = np.asarray(a.im.convert("L"), dtype=np.int16)
    gb = np.asarray(b.im.convert("L"), dtype=np.int16)
    x0, y0 = a.px(x, y)
    x1, y1 = a.px(x + w, y + h)
    ref = ga[y0:y1, x0:x1]
    r = int(reach * a.scale)
    best = None
    for dy in range(-r, r + 1):
        for dx in range(-r, r + 1):
            if y0 + dy < 0 or x0 + dx < 0:
                continue
            cand = gb[y0 + dy : y1 + dy, x0 + dx : x1 + dx]
            if cand.shape != ref.shape:
                continue
            err = float(np.abs(cand - ref).mean())
            if best is None or err < best[0]:
                best = (err, dx, dy)
    zero = float(np.abs(gb[y0:y1, x0:x1] - ref).mean())
    if best is None:
        sys.exit("区域超出图像")
    err, dx, dy = best
    print(f"最佳位移 dx={dx / a.scale:g}pt dy={dy / a.scale:g}pt，残差 {err:.2f}（不位移时 {zero:.2f}）")
    if dx == 0 and dy == 0 and err < 0.5:
        print("没有位移")
    elif err > 3:
        print("残差偏大：区域里的内容本身变了，不只是平移")


def cmd_grid(args):
    shot = Shot(args[0])
    step = float(args[2]) if len(args) > 2 else 50.0
    out = shot.im.convert("RGBA")
    layer = Image.new("RGBA", out.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(layer)
    label = font(int(10 * shot.scale))
    x_end, y_end = shot.pt(out.width, out.height)

    def ticks(start, end):
        v = (start // step + 1) * step
        while v < end:
            yield v
            v += step

    for x in ticks(shot.ox, x_end):
        px, _ = shot.px(x, 0)
        major = x % (step * 2) == 0
        draw.line((px, 0, px, out.height), fill=(230, 0, 160, 150 if major else 70), width=1)
        if major:
            draw.text((px + 3, 2), f"{x:g}", fill=(200, 0, 140, 255), font=label)
    for y in ticks(shot.oy, y_end):
        _, py = shot.px(0, y)
        major = y % (step * 2) == 0
        draw.line((0, py, out.width, py), fill=(230, 0, 160, 150 if major else 70), width=1)
        if major:
            draw.text((3, py + 2), f"{y:g}", fill=(200, 0, 140, 255), font=label)
    shot.im = Image.alpha_composite(out, layer).convert("RGB")
    shot.save(args[1])


def cmd_stack(args):
    dst, paths = args[0], args[1:]
    shots = [Shot(p) for p in paths]
    scale = shots[0].scale
    label = font(int(11 * scale))
    strip = int(18 * scale)
    width = max(s.im.width for s in shots)
    height = sum(s.im.height + strip for s in shots)
    out = Image.new("RGB", (width, height), (255, 255, 255))
    draw = ImageDraw.Draw(out)
    y = 0
    for path, shot in zip(paths, shots):
        draw.rectangle((0, y, width, y + strip), fill=(40, 40, 48))
        draw.text((int(4 * scale), y + int(3 * scale)), Path(path).stem, fill=(255, 255, 255), font=label)
        out.paste(shot.im, (0, y + strip))
        y += strip + shot.im.height
    out.save(dst)


def cmd_color(args):
    shot = Shot(args[0])
    x, y = nums(args[1:], 2)
    r, g, b = shot.im.getpixel(shot.px(x, y))
    print(f"#{r:02x}{g:02x}{b:02x}  rgb({r}, {g}, {b})")


COMMANDS = {
    "tag": cmd_tag,
    "crop": cmd_crop,
    "diff": cmd_diff,
    "shift": cmd_shift,
    "grid": cmd_grid,
    "stack": cmd_stack,
    "color": cmd_color,
}

if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] not in COMMANDS:
        sys.exit(__doc__)
    COMMANDS[sys.argv[1]](sys.argv[2:])
