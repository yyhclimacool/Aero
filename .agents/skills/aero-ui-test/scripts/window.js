// osascript -l JavaScript window.js <pid>
// 打印该进程最大的那个窗口「窗口号 x y 宽 高」，屏幕点、左上角原点；找不到时输出空。
// 窗口号给 `screencapture -l` 用；窗口停在屏幕外或被挡住时也列得出来。用 JXA 是为了不必编译任何东西。
ObjC.import("CoreGraphics");

function run(argv) {
  const pid = parseInt(argv[0], 10);
  const options = $.kCGWindowListOptionAll | $.kCGWindowListExcludeDesktopElements;
  const list = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(options, $.kCGNullWindowID))) || [];
  let best = null;
  for (const w of list) {
    const b = w.kCGWindowBounds;
    if (w.kCGWindowOwnerPID !== pid || !b || b.Width <= 300) continue;
    if (!best || b.Width * b.Height > best.kCGWindowBounds.Width * best.kCGWindowBounds.Height) best = w;
  }
  if (!best) return "";
  const b = best.kCGWindowBounds;
  return [best.kCGWindowNumber, b.X, b.Y, b.Width, b.Height].map(Math.round).join(" ");
}
