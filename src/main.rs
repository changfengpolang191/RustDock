// Rust + Win32 分层窗口 Dock
// 核心思路：WS_EX_LAYERED 窗口 + UpdateLayeredWindow 贴预乘 alpha 位图，
// 每帧用 tiny_skia 在内存里画好整条 Dock，再一次性推给窗口。
//
// 坐标系统一：layout() 算一遍，绘制 / 命中 / 鼠标穿透全部读它，
// 避免"画的是一套、点的是一套"。

#![windows_subsystem = "windows"]
#![allow(static_mut_refs)]

use std::ffi::OsStr;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use tiny_skia::{
    FillRule, FilterQuality, Paint, Path as SkPath, PathBuilder, Pixmap, PixmapPaint, Transform,
};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::shellapi::{
    ShellExecuteExW, APPBARDATA, ABM_GETTASKBARPOS, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, SHAppBarMessage,
};
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

// 全部按 96 DPI 的逻辑值写，运行时乘 scale
const BASE: f32 = 44.0;       // 常态图标边长
const HOVER_SCALE: f32 = 1.6; // 悬停放大倍率
const GAP: f32 = 8.0;     // 图标间距
const SIGMA: f32 = 64.0;  // 放大影响半径，越大越"连坐"
const PAD: f32 = 10.0;    // 背景条内边距
const RADIUS: f32 = 20.0; // 背景条圆角
const MARGIN: f32 = 8.0;  // 离任务栏/屏幕底的空隙
const BAR_ALPHA: u8 = 150; // 背景条透明度，越小越透
const ICON_SIZE: i32 = 256; // 从 exe 提取图标时请求的边长

struct Item {
    exe: String,
    icon: Option<Pixmap>,
}

/// 一次布局的完整结果：每个图标多大、摆在窗口的哪里、背景条在哪
struct Layout {
    sizes: Vec<f32>,
    xs: Vec<f32>,
    bar_x: f32,
    bar_y: f32,
    bar_w: f32,
    bar_h: f32,
    top: f32, // 最高那个图标的顶边，用来判断鼠标是否落在可点区
}

struct Dock {
    items: Vec<Item>,
    hover: f32,
    base: f32,
    max: f32,
    gap: f32,
    sigma: f32,
    pad: f32,
    radius: f32,
    win_w: i32,
    win_h: i32,
}

impl Dock {
    fn new(items: Vec<Item>, scale: f32) -> Self {
        let base = BASE * scale;
        let max = BASE * HOVER_SCALE * scale;
        let gap = GAP * scale;
        let pad = PAD * scale;

        // 窗口必须能装下"所有图标同时最大"的极端情况，否则放大时会被裁掉。
        // 常态只会用到中间一小条，其余是透明像素 + HTTRANSPARENT 穿透。
        let n = items.len().max(1) as f32;
        let widest = n * max + (n - 1.0).max(0.0) * gap + 2.0 * pad;

        Dock {
            items,
            hover: -1.0e6,
            base,
            max,
            gap,
            sigma: SIGMA * scale,
            pad,
            radius: RADIUS * scale,
            win_w: widest.ceil() as i32,
            win_h: (max + 2.0 * pad).ceil() as i32,
        }
    }

    /// 常态下的内容宽度（不含 pad）
    fn content_w(&self) -> f32 {
        let n = self.items.len().max(1) as f32;
        n * self.base + (n - 1.0).max(0.0) * self.gap
    }

    /// 不放大时背景条的左边 x。放大算法的距离基准就取这套坐标，
    /// 这样鼠标一动，图标是围绕固定中心"长"出来的，而不是互相推挤着漂移。
    fn base_bar_x(&self) -> f32 {
        (self.win_w as f32 - (self.content_w() + 2.0 * self.pad)) / 2.0
    }

    fn layout(&self) -> Layout {
        let n = self.items.len();
        let mut sizes = vec![0f32; n];

        // ── 第一趟：按鼠标到每个图标常态中心的距离算高斯衰减 ──
        let base_bar_x = self.base_bar_x();
        let two_sigma_sq = 2.0 * self.sigma * self.sigma;
        for (i, s) in sizes.iter_mut().enumerate() {
            let center = base_bar_x + self.pad + i as f32 * (self.base + self.gap) + self.base / 2.0;
            let d = self.hover - center;
            let k = (-(d * d) / two_sigma_sq).exp();
            *s = self.base + (self.max - self.base) * k;
        }

        // ── 第二趟：按新宽度重排 + 整体居中 ──
        // 只做第一趟会让图标互相压住，必须按实际尺寸重新铺一遍。
        let content: f32 = sizes.iter().sum::<f32>() + self.gap * (n as f32 - 1.0).max(0.0);
        let bar_w = content + 2.0 * self.pad;
        let bar_x = (self.win_w as f32 - bar_w) / 2.0;

        let mut xs = vec![0f32; n];
        let mut x = bar_x + self.pad;
        for i in 0..n {
            xs[i] = x;
            x += sizes[i] + self.gap;
        }

        let bar_h = self.base + 2.0 * self.pad;
        let bar_y = self.win_h as f32 - bar_h;
        let top = self.win_h as f32 - self.pad - self.max;

        Layout {
            sizes,
            xs,
            bar_x,
            bar_y,
            bar_w,
            bar_h,
            top,
        }
    }

    fn render(&self) -> Pixmap {
        let w = self.win_w.max(1) as u32;
        let h = self.win_h.max(1) as u32;
        let mut pm = match Pixmap::new(w, h) {
            Some(p) => p,
            None => return Pixmap::new(1, 1).unwrap(),
        };

        let l = self.layout();
        let win_h = self.win_h as f32;

        // ── 背景圆角条 ──
        let bg = rounded_rect(l.bar_x, l.bar_y, l.bar_w, l.bar_h, self.radius);
        let mut paint = Paint::default();
        paint.set_color_rgba8(24, 24, 28, BAR_ALPHA);
        paint.anti_alias = true;
        pm.as_mut()
            .fill_path(&bg, &paint, FillRule::Winding, Transform::identity(), None);

        // 顶部一道细高光，让它看起来像块玻璃而不是纯色块
        let gloss = rounded_rect(
            l.bar_x + 1.0,
            l.bar_y + 1.0,
            l.bar_w - 2.0,
            l.bar_h - 2.0,
            (self.radius - 1.0).max(0.0),
        );
        let mut gp = Paint::default();
        gp.set_color_rgba8(255, 255, 255, 18);
        gp.anti_alias = true;
        pm.as_mut()
            .fill_path(&gloss, &gp, FillRule::Winding, Transform::identity(), None);

        for (i, sz) in l.sizes.iter().copied().enumerate() {
            let x = l.xs[i];
            let y = win_h - self.pad - sz;

            match &self.items[i].icon {
                Some(icon) => {
                    let sx = sz / icon.width().max(1) as f32;
                    let sy = sz / icon.height().max(1) as f32;
                    let mut pp = PixmapPaint::default();
                    pp.quality = FilterQuality::Bicubic;
                    let src = icon.as_ref();
                    pm.as_mut().draw_pixmap(
                        0,
                        0,
                        src,
                        &pp,
                        Transform::from_scale(sx, sy).post_translate(x, y),
                        None,
                    );
                }
                None => {
                    // 没有图标文件时画个彩色圆角块占位
                    let block = rounded_rect(x, y, sz, sz, sz * 0.22);
                    let (r, g, b) = hsv_to_rgb((i as f32 * 47.0) % 360.0, 0.55, 0.85);
                    let mut bp = Paint::default();
                    bp.set_color_rgba8(r, g, b, 240);
                    bp.anti_alias = true;
                    pm.as_mut()
                        .fill_path(&block, &bp, FillRule::Winding, Transform::identity(), None);
                }
            }

            // 该程序有可见窗口时，图标下方画一个小圆点
            if is_running(&self.items[i].exe) {
                let cx = x + sz / 2.0;
                let cy = win_h - self.pad * 0.45;
                let r = (self.pad * 0.18).max(2.0);
                let mut pb = PathBuilder::new();
                pb.push_circle(cx, cy, r);
                if let Some(dot) = pb.finish() {
                    let mut dp = Paint::default();
                    dp.set_color_rgba8(235, 235, 245, 235);
                    dp.anti_alias = true;
                    pm.as_mut().fill_path(
                        &dot,
                        &dp,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        pm
    }

    /// 命中测试必须用放大后的真实矩形，否则鼠标一靠近图标就点歪
    fn hit(&self, px: f32, py: f32) -> Option<usize> {
        let l = self.layout();
        let win_h = self.win_h as f32;
        let slop = self.gap * 0.5; // 间隙也算到相邻图标上，手感更软
        for i in 0..self.items.len() {
            let sz = l.sizes[i];
            let x0 = l.xs[i] - slop;
            let y0 = win_h - self.pad - sz;
            if px >= x0 && px <= x0 + sz + 2.0 * slop && py >= y0 && py <= win_h {
                return Some(i);
            }
        }
        None
    }

    /// 这一像素要不要接鼠标。返回 false 就让系统把点击透给下面的窗口/桌面
    fn clickable(&self, px: f32, py: f32) -> bool {
        let l = self.layout();
        px >= l.bar_x - self.gap
            && px <= l.bar_x + l.bar_w + self.gap
            && py >= l.top - self.gap
            && py <= self.win_h as f32
    }
}

// ── 文字渲染 ──
// tiny-skia 只能画路径和图像，文字要靠 ab_glyph 把字形光栅化成覆盖率，
// 再逐像素混合进 Pixmap（预乘 RGBA）。
static mut FONT: Option<FontArc> = None;

fn font() -> Option<&'static FontArc> {
    unsafe {
        if FONT.is_none() {
            // 微软雅黑覆盖中英文；没有就退到 Segoe UI
            for path in [
                "C:\\Windows\\Fonts\\msyh.ttc",
                "C:\\Windows\\Fonts\\msyhl.ttc",
                "C:\\Windows\\Fonts\\segoeui.ttf",
            ] {
                if let Ok(data) = std::fs::read(path) {
                    if let Ok(f) = FontArc::try_from_vec(data) {
                        FONT = Some(f);
                        break;
                    }
                }
            }
            if FONT.is_none() {
                debug_log("font: 没有可用的字体文件");
            }
        }
        FONT.as_ref()
    }
}

/// 以覆盖率 cov 把 color 混合到 (x, y)。目标已是预乘，源也按预乘算
fn blend_px(pm: &mut Pixmap, x: i32, y: i32, color: [u8; 4], cov: f32) {
    if cov <= 0.0 {
        return;
    }
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let idx = ((y as u32 * w as u32 + x as u32) * 4) as usize;
    let sa = (color[3] as f32 / 255.0) * cov.min(1.0);
    if sa <= 0.0 {
        return;
    }
    let d = pm.data_mut();
    let sr = (color[0] as f32 / 255.0) * sa;
    let sg = (color[1] as f32 / 255.0) * sa;
    let sb = (color[2] as f32 / 255.0) * sa;
    let dr = d[idx] as f32 / 255.0;
    let dg = d[idx + 1] as f32 / 255.0;
    let db = d[idx + 2] as f32 / 255.0;
    let da = d[idx + 3] as f32 / 255.0;
    // 预乘空间的 over
    d[idx] = ((sr + dr * (1.0 - sa)) * 255.0) as u8;
    d[idx + 1] = ((sg + dg * (1.0 - sa)) * 255.0) as u8;
    d[idx + 2] = ((sb + db * (1.0 - sa)) * 255.0) as u8;
    d[idx + 3] = ((sa + da * (1.0 - sa)) * 255.0) as u8;
}

/// 画一行文字，top_y 是视觉顶部。返回结束时的 x
fn draw_text(pm: &mut Pixmap, text: &str, x: f32, top_y: f32, px: f32, color: [u8; 4]) -> f32 {
    let f = match font() {
        Some(f) => f,
        None => return x,
    };
    let scale = PxScale::from(px);
    let sfont = f.as_scaled(scale);
    let baseline = top_y + sfont.ascent();
    let mut caret = x;
    for ch in text.chars() {
        let gid = f.glyph_id(ch);
        let glyph = gid.with_scale_and_position(scale, ab_glyph::point(caret, baseline));
        if let Some(outline) = f.outline_glyph(glyph) {
            let bb = outline.px_bounds();
            outline.draw(|gx, gy, c| {
                blend_px(
                    pm,
                    bb.min.x as i32 + gx as i32,
                    bb.min.y as i32 + gy as i32,
                    color,
                    c,
                );
            });
        }
        caret += sfont.h_advance(gid);
    }
    caret
}

fn text_width(text: &str, px: f32) -> f32 {
    let f = match font() {
        Some(f) => f,
        None => return 0.0,
    };
    let scale = PxScale::from(px);
    let sfont = f.as_scaled(scale);
    text.chars().map(|c| sfont.h_advance(f.glyph_id(c))).sum()
}

fn now_hhmm() -> String {
    use winapi::um::minwinbase::SYSTEMTIME;
    use winapi::um::sysinfoapi::GetLocalTime;
    unsafe {
        let mut st: SYSTEMTIME = zeroed();
        GetLocalTime(&mut st);
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> SkPath {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    // tiny_skia 0.11 的 Path 没有 new()，finish() 只在路径为空时给 None。
    // 这里永远有闭合轮廓，兜底再拼一个同样的矩形，不依赖 unwrap 的运气。
    pb.finish().unwrap_or_else(|| {
        let mut b = PathBuilder::new();
        b.move_to(x, y);
        b.line_to(x + w, y);
        b.line_to(x + w, y + h);
        b.line_to(x, y + h);
        b.close();
        b.finish().expect("rect path is never empty")
    })
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (
        ((r1 + m) * 255.0) as u8,
        ((g1 + m) * 255.0) as u8,
        ((b1 + m) * 255.0) as u8,
    )
}

// ── 把 tiny_skia 的 RGBA 预乘位图塞进分层窗口 ──
unsafe fn present(hwnd: HWND, pm: &Pixmap) {
    let w = pm.width() as i32;
    let h = pm.height() as i32;
    if w <= 0 || h <= 0 {
        return;
    }

    let screen = GetDC(null_mut());
    if screen.is_null() {
        return;
    }
    let mem = CreateCompatibleDC(screen);
    if mem.is_null() {
        ReleaseDC(null_mut(), screen);
        return;
    }

    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // 负数 = top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    // winapi 的 c_void 是它自己在 lib.rs 里定义的 enum，和 std::ffi::c_void 是两个类型，
    // 混用直接编译不过。这里一律用 winapi 的 LPVOID。
    let mut bits: LPVOID = null_mut();
    let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if dib.is_null() || bits.is_null() {
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        return;
    }
    let old = SelectObject(mem, dib as HGDIOBJ);

    // RGBA -> BGRA（UpdateLayeredWindow 要 BGRA，且 tiny_skia 输出已是预乘 alpha）
    let src = pm.data();
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    let n = dst.len().min(src.len());
    let mut i = 0;
    while i + 3 < n {
        dst[i] = src[i + 2];
        dst[i + 1] = src[i + 1];
        dst[i + 2] = src[i];
        dst[i + 3] = src[i + 3];
        i += 4;
    }

    let mut size = SIZE { cx: w, cy: h };
    let mut src_pt = POINT { x: 0, y: 0 };
    let mut blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    UpdateLayeredWindow(
        hwnd,
        screen,
        null_mut(), // 位置不变，靠 SetWindowPos 定
        &mut size,
        mem,
        &mut src_pt,
        0,
        &mut blend,
        ULW_ALPHA,
    );

    SelectObject(mem, old);
    DeleteObject(dib as HGDIOBJ);
    DeleteDC(mem);
    ReleaseDC(null_mut(), screen);
}

static mut DOCK: Option<Dock> = None;

// 记录桌面图标是不是被我们隐藏的，退出时好恢复
static mut HID_DESKTOP_ICONS: bool = false;

// 通知器重启的消息（explorer 重启后任务栏会重新出现）
static mut TASKBAR_CREATED_MSG: UINT = 0;
static mut TASKBAR_HIDDEN: bool = false;
static mut TASKBAR_ORIG: RECT = RECT {
    left: 0,
    top: 0,
    right: 0,
    bottom: 0,
};

unsafe fn taskbar_window() -> HWND {
    FindWindowW(wide("Shell_TrayWnd").as_ptr(), null())
}

/// 隐藏任务栏：直接藏窗口。
/// 只把它移出屏幕不够 —— Win11 的自动隐藏在鼠标触底时会把它拉回来显示，
/// 所以这里改成 ShowWindow(SW_HIDE)。
unsafe fn hide_taskbar() {
    let tray = taskbar_window();
    if tray.is_null() {
        return;
    }
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let mut rc: RECT = zeroed();
    if GetWindowRect(tray, &mut rc) != 0 && rc.top < sh - 2 {
        TASKBAR_ORIG = rc;
    }
    ShowWindow(tray, SW_HIDE);
    TASKBAR_HIDDEN = true;
    debug_log(&format!(
        "taskbar: 已隐藏，原位置 top={}",
        TASKBAR_ORIG.top
    ));
}

unsafe fn show_taskbar() {
    if !TASKBAR_HIDDEN {
        return;
    }
    let tray = taskbar_window();
    if tray.is_null() {
        return;
    }
    ShowWindow(tray, SW_SHOW);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let top = if TASKBAR_ORIG.top > 0 && TASKBAR_ORIG.top <= sh {
        TASKBAR_ORIG.top
    } else {
        sh - 48
    };
    SetWindowPos(
        tray,
        null_mut(),
        TASKBAR_ORIG.left,
        top,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
    );
    TASKBAR_HIDDEN = false;
}

// 当前有可见窗口的 exe 路径集合，用来在 Dock 上画运行指示点
static mut RUNNING: Vec<String> = Vec::new();
static mut DOCK_HWND: HWND = null_mut();

unsafe extern "system" fn collect_running_proc(hwnd: HWND, _: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    if !GetWindow(hwnd, GW_OWNER).is_null() {
        return 1;
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
    if ex & (WS_EX_TOOLWINDOW as i32) != 0 {
        return 1;
    }
    let mut pid: DWORD = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != 0 {
        if let Some(p) = process_image_path(pid) {
            if !RUNNING.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
                RUNNING.push(p);
            }
        }
    }
    1
}

unsafe fn refresh_running() {
    RUNNING.clear();
    EnumWindows(Some(collect_running_proc), 0);
}

fn is_running(exe: &str) -> bool {
    unsafe { RUNNING.iter().any(|x| x.eq_ignore_ascii_case(exe)) }
}

unsafe fn redraw(hwnd: HWND) {
    if let Some(dock) = DOCK.as_ref() {
        let pm = dock.render();
        present(hwnd, &pm);
    }
}

// 找窗口时用来传参（EnumWindows 的回调不能捕获环境）
static mut MATCH_EXE: Option<String> = None;
static mut MATCH_HWND: HWND = null_mut();

/// 拿进程的 exe 完整路径
unsafe fn process_image_path(pid: DWORD) -> Option<String> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winbase::QueryFullProcessImageNameW;

    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    let mut size = buf.len() as DWORD;
    let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
    CloseHandle(h);
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

unsafe extern "system" fn match_window_proc(hwnd: HWND, _: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    // 只要无 owner 的顶层窗口
    if !GetWindow(hwnd, GW_OWNER).is_null() {
        return 1;
    }
    // 跳过工具窗口（托盘图标之类）
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
    if ex & (WS_EX_TOOLWINDOW as i32) != 0 {
        return 1;
    }

    let mut pid: DWORD = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return 1;
    }
    if let Some(path) = process_image_path(pid) {
        if let Some(target) = MATCH_EXE.as_ref() {
            if path.eq_ignore_ascii_case(target) {
                MATCH_HWND = hwnd;
                return 0; // 找到就停
            }
        }
    }
    1
}

/// 按 exe 完整路径找该程序的主窗口（不看 PID，程序重启过也能找到）
unsafe fn find_window_by_exe(exe: &str) -> Option<HWND> {
    MATCH_EXE = Some(exe.to_string());
    MATCH_HWND = null_mut();
    EnumWindows(Some(match_window_proc), 0);
    MATCH_EXE = None;
    if MATCH_HWND.is_null() {
        None
    } else {
        Some(MATCH_HWND)
    }
}

/// 点图标：已经在跑就最小化/还原，没在跑就启动
unsafe fn toggle_app(exe: &str) {
    if let Some(hwnd) = find_window_by_exe(exe) {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_MINIMIZE);
        }
        SetForegroundWindow(hwnd);
        debug_log(&format!("toggle: {} -> 切换窗口", exe));
    } else {
        launch(exe);
    }
}

fn launch(cmd: &str) {
    let op: Vec<u16> = OsStr::new("open").encode_wide().chain(Some(0)).collect();
    let file: Vec<u16> = OsStr::new(cmd).encode_wide().chain(Some(0)).collect();
    unsafe {
        // 注意：不要用 explorer.exe 当代理。ShellExecute("explorer.exe", 目标)
        // 实际是 CreateProcess 一个新 explorer 进程，而 explorer 必须由 shell
        // 会话管理，结果必定 0xc0000142。启动目标只能直连。
        let mut sei: SHELLEXECUTEINFOW = zeroed();
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as DWORD;
        sei.fMask = SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC;
        sei.lpVerb = op.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut sei) == 0 {
            let e = std::io::Error::last_os_error();
            debug_log(&format!("launch 失败: {} ({})", cmd, e));
        } else {
            debug_log(&format!("launch 成功: {}", cmd));
        }
    }
}

/// 设 RUSTDOCK_DEBUG=1 时把定位过程写到 %TEMP%\rustdock.log，排查几何问题用
/// 把关键过程写到 exe 旁的 rustdock.log。每次启动会重写，方便排查
/// 图标提取和"点了没反应"这类问题（用户不用去配环境变量）。
fn debug_log(msg: &str) {
    use std::io::Write;
    let p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("rustdock.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("rustdock.log"));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{}", msg);
    }
}

/// 从注册表读任务栏停靠矩形。
///
/// explorer 把 APPBARDATA 序列化在
/// HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StuckRects3\Settings，
/// 48 字节的二进制里 offset 24 起是 rc(LEFT, TOP, RIGHT, BOTTOM)。
/// 任务栏自动隐藏时它的窗口被推到屏幕外，只有这里记得真实停靠位置。
unsafe fn taskbar_top_from_registry(screen_w: i32, screen_h: i32) -> Option<i32> {
    use winapi::um::winnt::KEY_READ;
    use winapi::um::winreg::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER};

    let sub: Vec<u16> =
        OsStr::new("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StuckRects3")
            .encode_wide()
            .chain(Some(0))
            .collect();
    let val: Vec<u16> = OsStr::new("Settings").encode_wide().chain(Some(0)).collect();

    let mut hkey: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
        debug_log("registry: 打开 StuckRects3 失败");
        return None;
    }

    let mut buf = [0u8; 128];
    let mut len: DWORD = buf.len() as DWORD;
    let mut ty: DWORD = 0;
    let rc = RegQueryValueExW(
        hkey,
        val.as_ptr(),
        null_mut(),
        &mut ty,
        buf.as_mut_ptr(),
        &mut len,
    );
    RegCloseKey(hkey);
    if rc != 0 || len < 40 {
        debug_log(&format!("registry: 读 Settings 失败 rc={} len={}", rc, len));
        return None;
    }

    let rd = |o: usize| i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let (left, top, right, bottom) = (rd(24), rd(28), rd(32), rd(36));
    debug_log(&format!(
        "registry_taskbar=({},{})-({},{}) len={}",
        left, top, right, bottom, len
    ));

    // 严格校验，只认横跨屏幕、贴在下半部、底边就是屏幕底的任务栏
    if right - left > screen_w / 2 && top > screen_h / 2 && bottom >= screen_h - 8 {
        Some(top)
    } else {
        None
    }
}

/// Dock 底边参考线：让开任务栏，再留 MARGIN 的空隙。
///
/// 任务栏是自动隐藏的（ABS_AUTOHIDE），这时候：
///   - SPI_GETWORKAREA 返回整屏，等于没让位；
///   - Shell_TrayWnd 窗口被推到屏幕外（实测 y=1598），也是错的；
///   - SHAppBarMessage(ABM_GETTASKBARPOS) 在 PowerShell 里能拿到正确矩形，
///     但在本进程里直接返回 0（实测 appbar_ret=0），不能用；
///   - 注册表 StuckRects3 里存的是同样的停靠矩形，和正确值一致。
/// 所以顺序是：注册表 → SHAppBarMessage → 抓窗口，前一个不成立才退下一个。
unsafe fn dock_base_y(scale: f32) -> i32 {
    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);
    let mut bottom = screen_h;
    let mut src = "screen";

    if let Some(top) = taskbar_top_from_registry(screen_w, screen_h) {
        bottom = top;
        src = "registry";
    }

    if src == "screen" {
        let mut abd: APPBARDATA = zeroed();
        abd.cbSize = size_of::<APPBARDATA>() as DWORD;
        let ret = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        debug_log(&format!(
            "appbar_ret={} rc=({},{})-({},{})",
            ret, abd.rc.left, abd.rc.top, abd.rc.right, abd.rc.bottom
        ));
        if ret != 0 {
            let rc = abd.rc;
            let bar_w = rc.right - rc.left;
            let bar_h = rc.bottom - rc.top;
            if bar_w > screen_w / 2 && bar_h > 0 && bar_h < screen_h / 3 && rc.top > screen_h / 2 {
                bottom = rc.top;
                src = "appbar";
            }
        }
    }

    if src == "screen" {
        let cls: Vec<u16> = OsStr::new("Shell_TrayWnd")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let tray = FindWindowW(cls.as_ptr(), null());
        if !tray.is_null() {
            let mut rc: RECT = zeroed();
            if GetWindowRect(tray, &mut rc) != 0 {
                let bar_w = rc.right - rc.left;
                let bar_h = rc.bottom - rc.top;
                debug_log(&format!(
                    "traywnd_rc=({},{})-({},{})",
                    rc.left, rc.top, rc.right, rc.bottom
                ));
                if bar_w > screen_w / 2
                    && bar_h > 0
                    && bar_h < screen_h / 3
                    && rc.top > screen_h / 2
                {
                    bottom = rc.top;
                    src = "traywnd";
                }
            }
        }
    }

    // 工作区也可能更小（多屏、被其他 AppBar 挤过），取更保守的那个
    let mut work: RECT = zeroed();
    if SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as LPVOID, 0) != 0 {
        let w = work.right - work.left;
        let h = work.bottom - work.top;
        if w > 0 && h > 0 {
            debug_log(&format!(
                "workarea=({},{})-({},{})",
                work.left, work.top, work.right, work.bottom
            ));
            if work.bottom < bottom {
                bottom = work.bottom;
                src = "workarea";
            }
        }
    }

    let base = (bottom - (MARGIN * scale) as i32).max(0);
    debug_log(&format!(
        "base_y={} bottom={} src={} scale={}",
        base, bottom, src, scale
    ));
    base
}

fn px(lparam: LPARAM) -> f32 {
    (lparam & 0xFFFF) as i16 as f32
}

fn py(lparam: LPARAM) -> f32 {
    ((lparam >> 16) & 0xFFFF) as i16 as f32
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 分层窗口自己管绘制，别让系统插一脚，否则会闪
        WM_ERASEBKGND => 1,
        WM_PAINT => 0,

        // 窗口里有大片透明区，不返回 HTTRANSPARENT 就会挡住底下的桌面和别的窗口
        WM_NCHITTEST => {
            let mut pt = POINT {
                x: px(lparam) as i32,
                y: py(lparam) as i32,
            };
            ScreenToClient(hwnd, &mut pt);
            let inside = match DOCK.as_ref() {
                Some(d) => d.clickable(pt.x as f32, pt.y as f32),
                None => false,
            };
            if inside {
                HTCLIENT as LRESULT
            } else {
                HTTRANSPARENT as LRESULT
            }
        }

        WM_MOUSEMOVE => {
            let x = px(lparam);

            // 注册一次，离开窗口才会收到 WM_MOUSELEAVE
            let mut tme: TRACKMOUSEEVENT = zeroed();
            tme.cbSize = size_of::<TRACKMOUSEEVENT>() as u32;
            tme.dwFlags = TME_LEAVE;
            tme.hwndTrack = hwnd;
            TrackMouseEvent(&mut tme);

            if let Some(dock) = DOCK.as_mut() {
                if (dock.hover - x).abs() > 0.5 {
                    dock.hover = x;
                    redraw(hwnd);
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            if let Some(dock) = DOCK.as_mut() {
                dock.hover = -1.0e6;
                redraw(hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            let (cx, cy) = (px(lparam), py(lparam));
            let target = match DOCK.as_ref() {
                Some(dock) => dock.hit(cx, cy).map(|i| dock.items[i].exe.clone()),
                None => None,
            };
            if let Some(exe) = target {
                toggle_app(&exe);
            }
            0
        }
        // 右键菜单
        WM_RBUTTONUP => {
            let (cx, cy) = (px(lparam), py(lparam));
            let item = match DOCK.as_ref() {
                Some(d) => d.hit(cx, cy).map(|i| d.items[i].exe.clone()),
                None => None,
            };

            let menu = CreatePopupMenu();
            if menu.is_null() {
                return 0;
            }
            if item.is_some() {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    IDM_OPEN_LOCATION,
                    wide("打开文件位置").as_ptr(),
                );
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            }
            AppendMenuW(menu, MF_STRING, IDM_QUIT, wide("退出 Dock").as_ptr());

            let mut pt = POINT {
                x: cx as i32,
                y: cy as i32,
            };
            ClientToScreen(hwnd, &mut pt);
            // 前置一下，否则点击菜单外部时菜单不会收起
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                hwnd,
                null_mut(),
            );
            DestroyMenu(menu);

            match cmd as usize {
                IDM_OPEN_LOCATION => {
                    if let Some(exe) = item {
                        open_file_location(&exe);
                    }
                }
                IDM_QUIT => PostQuitMessage(0),
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            show_taskbar();
            if HID_DESKTOP_ICONS {
                set_desktop_icons_hidden(false);
                HID_DESKTOP_ICONS = false;
            }
            PostQuitMessage(0);
            0
        }
        m => {
            // explorer 重启会重建任务栏，收到通知再藏一次
            if TASKBAR_CREATED_MSG != 0 && m == TASKBAR_CREATED_MSG {
                hide_taskbar();
                return 0;
            }
            DefWindowProcW(hwnd, m, wparam, lparam)
        }
    }
}

// ── 从 exe 里抽图标 ──
// winapi 0.3.9 没导出 PrivateExtractIconsW，自己声明。
// 它能直接指定 256x256，比 ExtractIconEx 强（后者只有 16/32/48，放到 150% 屏上必糊）。
#[link(name = "user32")]
extern "system" {
    fn PrivateExtractIconsW(
        szFileName: *const u16,
        nIconIndex: i32,
        cxIcon: i32,
        cyIcon: i32,
        phicon: *mut HICON,
        piconid: *mut UINT,
        nIcons: UINT,
        flags: UINT,
    ) -> UINT;
}

/// HICON -> tiny_skia 预乘 RGBA 位图
unsafe fn hicon_to_pixmap(hicon: HICON) -> Option<Pixmap> {
    let mut ii: ICONINFO = zeroed();
    if GetIconInfo(hicon, &mut ii) == 0 {
        return None;
    }

    let screen = GetDC(null_mut());
    if screen.is_null() {
        DeleteObject(ii.hbmColor as HGDIOBJ);
        DeleteObject(ii.hbmMask as HGDIOBJ);
        return None;
    }

    // 请求 256 不代表真拿到 256，以位图实际尺寸为准
    let mut bm: BITMAP = zeroed();
    let got = GetObjectW(
        ii.hbmColor as HGDIOBJ,
        size_of::<BITMAP>() as i32,
        &mut bm as *mut BITMAP as LPVOID,
    );
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        DeleteObject(ii.hbmColor as HGDIOBJ);
        DeleteObject(ii.hbmMask as HGDIOBJ);
        ReleaseDC(null_mut(), screen);
        return None;
    }
    let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);

    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w as i32;
    bmi.bmiHeader.biHeight = -(h as i32); // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        screen,
        ii.hbmColor,
        0,
        h,
        buf.as_mut_ptr() as LPVOID,
        &mut bmi,
        DIB_RGB_COLORS,
    );

    // 老式 32bpp 图标的 alpha 通道全是 0，得从 mask 反推：mask 白 = 该像素透明
    if lines != 0 {
        let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
        if !has_alpha {
            let mut mask = vec![0u8; (w * h * 4) as usize];
            let ml = GetDIBits(
                screen,
                ii.hbmMask,
                0,
                h,
                mask.as_mut_ptr() as LPVOID,
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if ml != 0 {
                for i in 0..(w * h) as usize {
                    buf[i * 4 + 3] = if mask[i * 4] > 127 { 0 } else { 255 };
                }
            }
        }
    }

    DeleteObject(ii.hbmColor as HGDIOBJ);
    DeleteObject(ii.hbmMask as HGDIOBJ);
    ReleaseDC(null_mut(), screen);

    if lines == 0 {
        return None;
    }

    let mut pm = Pixmap::new(w, h)?;
    let dst = pm.data_mut();
    for i in 0..(w * h) as usize {
        let b = buf[i * 4] as u32;
        let g = buf[i * 4 + 1] as u32;
        let r = buf[i * 4 + 2] as u32;
        let a = buf[i * 4 + 3];
        // tiny_skia 要预乘 alpha，GetDIBits 给的是直通
        let af = a as u32;
        dst[i * 4] = ((r * af) / 255) as u8;
        dst[i * 4 + 1] = ((g * af) / 255) as u8;
        dst[i * 4 + 2] = ((b * af) / 255) as u8;
        dst[i * 4 + 3] = a;
    }
    Some(pm)
}

unsafe fn icon_from_exe(exe: &str) -> Option<Pixmap> {
    let path: Vec<u16> = OsStr::new(exe).encode_wide().chain(Some(0)).collect();
    let mut hicon: HICON = null_mut();
    let n = PrivateExtractIconsW(
        path.as_ptr(),
        0,
        ICON_SIZE,
        ICON_SIZE,
        &mut hicon,
        null_mut(),
        1,
        0,
    );
    if n == 0 || hicon.is_null() {
        debug_log(&format!("icon: 提取失败 {}", exe));
        return None;
    }
    let pm = hicon_to_pixmap(hicon);
    DestroyIcon(hicon);
    match &pm {
        Some(p) => debug_log(&format!("icon: {} -> {}x{}", exe, p.width(), p.height())),
        None => debug_log(&format!("icon: {} 转位图失败", exe)),
    }
    pm
}

fn load_items() -> Vec<Item> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    // 优先 exe 同目录；找不到再退回当前工作目录。
    // 开发时 exe 在 target/release/ 下、apps.txt 在项目根，不回退就永远读不到配置，
    // 会静默退化到内置的默认项。
    let mut cfg_path = exe_dir.join("apps.txt");
    if !cfg_path.exists() {
        if let Ok(cwd) = std::env::current_dir() {
            let cand = cwd.join("apps.txt");
            if cand.exists() {
                cfg_path = cand;
            }
        }
    }
    // 图标里的相对路径按 apps.txt 所在目录算，发布时整包放一起、开发时放项目根都对
    let base_dir = cfg_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| exe_dir.clone());

    let mut items = Vec::new();

    // apps.txt：每行  可执行文件或路径|图标png相对路径
    if let Ok(text) = std::fs::read_to_string(&cfg_path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split('|');
            let exe = parts.next().unwrap_or("").trim().to_string();
            if exe.is_empty() {
                continue;
            }
            let icon_spec = parts
                .next()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());

            let mut icon = icon_spec.as_deref().and_then(|p| {
                let pb = if Path::new(p).is_absolute() {
                    PathBuf::from(p)
                } else {
                    base_dir.join(p)
                };
                // 图标列写 .exe = "从这个程序里抽图标"，跳过 png 加载
                if pb
                    .extension()
                    .map_or(false, |e| e.eq_ignore_ascii_case("exe"))
                {
                    return None;
                }
                Pixmap::load_png(&pb).ok()
            });

            if icon.is_none() {
                // 图标列是 exe 就抽那个程序的图标，否则抽启动目标自己的
                let src = icon_spec
                    .as_deref()
                    .filter(|p| p.to_ascii_lowercase().ends_with(".exe"))
                    .unwrap_or(exe.as_str());
                icon = unsafe { icon_from_exe(src) };
            }
            items.push(Item { exe, icon });
        }
    }

    // 没有配置就给几个默认项，图标缺失时画色块
    if items.is_empty() {
        let defaults = [
            ("C:\\Windows\\explorer.exe", "explorer"),
            ("C:\\Windows\\System32\\notepad.exe", "notepad"),
            ("C:\\Windows\\System32\\mspaint.exe", "mspaint"),
            ("C:\\Windows\\System32\\cmd.exe", "cmd"),
            ("C:\\Windows\\System32\\control.exe", "control"),
        ];
        for (exe, name) in defaults {
            let png = base_dir.join("assets").join(format!("{name}.png"));
            let icon = Pixmap::load_png(&png)
                .ok()
                .or_else(|| unsafe { icon_from_exe(exe) });
            items.push(Item {
                exe: exe.to_string(),
                icon,
            });
        }
    }

    items
}

/// 开机自启：写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run
unsafe fn set_autostart(enable: bool) -> bool {
    use winapi::um::winnt::{KEY_SET_VALUE, REG_SZ};
    use winapi::um::winreg::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
    };

    let sub: Vec<u16> =
        OsStr::new("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .encode_wide()
            .chain(Some(0))
            .collect();
    let name: Vec<u16> = OsStr::new("RustDock")
        .encode_wide()
        .chain(Some(0))
        .collect();

    let mut hkey: HKEY = null_mut();
    let ret = RegCreateKeyExW(
        HKEY_CURRENT_USER,
        sub.as_ptr(),
        0,
        null_mut(),
        0,
        KEY_SET_VALUE,
        null_mut(),
        &mut hkey,
        null_mut(),
    );
    if ret != 0 {
        debug_log(&format!("autostart: RegCreateKeyExW 失败 ret={}", ret));
        return false;
    }

    let ok = if enable {
        match std::env::current_exe() {
            Ok(exe) => {
                // 路径带空格时必须加引号，否则开机时会被系统拆成多个参数
                let cmd = format!("\"{}\"", exe.display());
                let data: Vec<u16> = OsStr::new(&cmd).encode_wide().chain(Some(0)).collect();
                let bytes = (data.len() * 2) as DWORD;
                let r = RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    bytes,
                );
                if r != 0 {
                    debug_log(&format!("autostart: RegSetValueExW 失败 ret={}", r));
                }
                r == 0
            }
            Err(_) => false,
        }
    } else {
        RegDeleteValueW(hkey, name.as_ptr()) == 0
    };

    RegCloseKey(hkey);
    ok
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// 桌面图标容器窗口。Win11 里它可能挂在 Progman 下，也可能挂在某个 WorkerW 下
unsafe fn desktop_defview() -> HWND {
    let progman = FindWindowW(wide("Progman").as_ptr(), null());
    if !progman.is_null() {
        let dv = FindWindowExW(progman, null_mut(), wide("SHELLDLL_DefView").as_ptr(), null());
        if !dv.is_null() {
            return dv;
        }
    }
    let mut ww = FindWindowExW(null_mut(), null_mut(), wide("WorkerW").as_ptr(), null());
    while !ww.is_null() {
        let dv = FindWindowExW(ww, null_mut(), wide("SHELLDLL_DefView").as_ptr(), null());
        if !dv.is_null() {
            return dv;
        }
        ww = FindWindowExW(null_mut(), ww, wide("WorkerW").as_ptr(), null());
    }
    null_mut()
}

unsafe fn read_hide_icons() -> u32 {
    use winapi::um::winnt::{KEY_READ, REG_DWORD};
    use winapi::um::winreg::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER};

    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced");
    let name = wide("HideIcons");
    let mut hkey: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
        return 0;
    }
    let mut val: DWORD = 0;
    let mut len: DWORD = size_of::<DWORD>() as DWORD;
    let mut ty: DWORD = 0;
    let r = RegQueryValueExW(
        hkey,
        name.as_ptr(),
        null_mut(),
        &mut ty,
        &mut val as *mut DWORD as *mut u8,
        &mut len,
    );
    RegCloseKey(hkey);
    if r == 0 && ty == REG_DWORD {
        val
    } else {
        0
    }
}

unsafe fn write_hide_icons(v: u32) -> bool {
    use winapi::um::winnt::{KEY_SET_VALUE, REG_DWORD};
    use winapi::um::winreg::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER,
    };

    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced");
    let name = wide("HideIcons");
    let mut hkey: HKEY = null_mut();
    if RegCreateKeyExW(
        HKEY_CURRENT_USER,
        sub.as_ptr(),
        0,
        null_mut(),
        0,
        KEY_SET_VALUE,
        null_mut(),
        &mut hkey,
        null_mut(),
    ) != 0
    {
        return false;
    }
    let val: DWORD = v;
    let r = RegSetValueExW(
        hkey,
        name.as_ptr(),
        0,
        REG_DWORD,
        &val as *const DWORD as *const u8,
        size_of::<DWORD>() as DWORD,
    );
    RegCloseKey(hkey);
    r == 0
}

/// 桌面图标开关。0x7402 是 explorer 内部"显示桌面图标"的切换命令。
const DESKTOP_ICONS_CMD: WPARAM = 0x7402;

unsafe fn toggle_desktop_icons() -> bool {
    let dv = desktop_defview();
    if dv.is_null() {
        return false;
    }
    SendMessageW(dv, WM_COMMAND, DESKTOP_ICONS_CMD, 0);
    true
}

/// 让桌面图标状态变成 hide 指定的样子。注册表和 DefView 一起改，保持同步。
unsafe fn set_desktop_icons_hidden(hide: bool) -> bool {
    let want = if hide { 1 } else { 0 };
    let cur = read_hide_icons();
    if !write_hide_icons(want) {
        debug_log("desktop-icons: 写 HideIcons 失败");
        return false;
    }
    if cur == want {
        return true;
    }
    toggle_desktop_icons()
}

// ── 顶部栏 ──
const TOPBAR_H: f32 = 26.0;
const TOPBAR_ALPHA: u8 = 170;
const TOPBAR_FONT_PX: f32 = 12.5;

// ── 系统音量（Core Audio）──
// winapi 的 RIDL! 不生成 IID_XXX 常量，这里按 uuid 手写
const IID_IMMDEVICEENUMERATOR: winapi::shared::guiddef::GUID = winapi::shared::guiddef::GUID {
    Data1: 0xa95664d2,
    Data2: 0x9614,
    Data3: 0x4f35,
    Data4: [0xa7, 0x46, 0xde, 0x8d, 0xb6, 0x36, 0x17, 0xe6],
};
const IID_IAUDIOENDPOINTVOLUME: winapi::shared::guiddef::GUID = winapi::shared::guiddef::GUID {
    Data1: 0x5cdf2c82,
    Data2: 0x841e,
    Data3: 0x4546,
    Data4: [0x97, 0x22, 0x0c, 0xf7, 0x40, 0x78, 0x22, 0x9a],
};

static mut VOLUME_CTL: *mut winapi::um::endpointvolume::IAudioEndpointVolume = null_mut();

unsafe fn volume_ctl() -> *mut winapi::um::endpointvolume::IAudioEndpointVolume {
    use winapi::um::combaseapi::{CoCreateInstance, CLSCTX_ALL};
    use winapi::um::endpointvolume::IAudioEndpointVolume;
    use winapi::um::mmdeviceapi::{
        eConsole, eRender, IMMDevice, IMMDeviceEnumerator, CLSID_MMDeviceEnumerator,
    };

    if !VOLUME_CTL.is_null() {
        return VOLUME_CTL;
    }

    let mut enumr: *mut IMMDeviceEnumerator = null_mut();
    let hr = CoCreateInstance(
        &CLSID_MMDeviceEnumerator,
        null_mut(),
        CLSCTX_ALL,
        &IID_IMMDEVICEENUMERATOR,
        &mut enumr as *mut _ as *mut _,
    );
    if hr != 0 || enumr.is_null() {
        debug_log(&format!("volume: CoCreateInstance 失败 hr=0x{:X}", hr));
        return null_mut();
    }

    let mut dev: *mut IMMDevice = null_mut();
    let hr = (*enumr).GetDefaultAudioEndpoint(eRender, eConsole, &mut dev);
    if hr != 0 || dev.is_null() {
        debug_log(&format!("volume: 取默认设备失败 hr=0x{:X}", hr));
        return null_mut();
    }

    let mut vol: *mut IAudioEndpointVolume = null_mut();
    let hr = (*dev).Activate(
        &IID_IAUDIOENDPOINTVOLUME,
        CLSCTX_ALL,
        null_mut(),
        &mut vol as *mut _ as *mut _,
    );
    if hr != 0 || vol.is_null() {
        debug_log(&format!("volume: Activate 失败 hr=0x{:X}", hr));
        return null_mut();
    }
    VOLUME_CTL = vol;
    vol
}

/// 主音量百分比 0~100
unsafe fn system_volume() -> Option<u8> {
    let c = volume_ctl();
    if c.is_null() {
        return None;
    }
    let mut v: f32 = 0.0;
    if (*c).GetMasterVolumeLevelScalar(&mut v) != 0 {
        return None;
    }
    Some((v * 100.0).round().clamp(0.0, 100.0) as u8)
}

unsafe fn is_muted() -> bool {
    let c = volume_ctl();
    if c.is_null() {
        return false;
    }
    let mut m: BOOL = 0;
    (*c).GetMute(&mut m) == 0 && m != 0
}

unsafe fn toggle_mute() {
    let c = volume_ctl();
    if c.is_null() {
        return;
    }
    let mut m: BOOL = 0;
    if (*c).GetMute(&mut m) == 0 {
        (*c).SetMute(if m != 0 { 0 } else { 1 }, null());
    }
}

/// 相对调节音量，delta 是 0~1 的增量
unsafe fn adjust_volume(delta: f32) {
    let c = volume_ctl();
    if c.is_null() {
        return;
    }
    let mut v: f32 = 0.0;
    if (*c).GetMasterVolumeLevelScalar(&mut v) != 0 {
        return;
    }
    let next = (v + delta).clamp(0.0, 1.0);
    (*c).SetMasterVolumeLevelScalar(next, null());
    // 调音量时自动解除静音，否则滚半天没声音
    let mut m: BOOL = 0;
    if (*c).GetMute(&mut m) == 0 && m != 0 {
        (*c).SetMute(0, null());
    }
}

const IDM_OPEN_LOCATION: usize = 1;
const IDM_QUIT: usize = 2;

/// 在资源管理器里打开文件所在目录（目录则直接打开）
fn open_file_location(path: &str) {
    let p = Path::new(path);
    let target = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| p.to_path_buf())
    };
    launch(&target.display().to_string());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    NewFile,
    NewFolder,
    ShowDesktops,
    NewDesktop,
    Help,
    ToggleMute,
}

struct TopBar {
    buttons: Vec<(&'static str, Act)>,
    hover: Option<usize>,
    win_w: i32,
    win_h: i32,
    pad: f32,
    gap: f32,
    font_px: f32,
    // 桌面缩略图组的位置与尺寸（绘制和命中共用）
    desk_x: f32,
    desk_w: f32,
    desk_h: f32,
    plus_x: f32,
    plus_w: f32,
}

impl TopBar {
    fn new(scale: f32) -> Self {
        let win_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let pad = 14.0 * scale;
        let gap = 22.0 * scale;
        let font_px = TOPBAR_FONT_PX * scale;
        let dk_w = 16.0 * scale;
        let dk_h = 10.0 * scale;

        // 先量出前两个按钮的宽度，桌面缩略图跟在它们后面
        let b1 = text_width("新建文件", font_px);
        let b2 = text_width("新建文件夹", font_px);
        let desk_x = pad + b1 + gap + b2 + gap;
        let desk_span = dk_w + 5.0 * scale * 2.0; // 两个层叠偏移
        let plus_x = desk_x + desk_span + 8.0 * scale;

        TopBar {
            buttons: vec![
                ("新建文件", Act::NewFile),
                ("新建文件夹", Act::NewFolder),
                ("帮助", Act::Help),
            ],
            hover: None,
            win_w,
            win_h: (TOPBAR_H * scale).ceil() as i32,
            pad,
            gap,
            font_px,
            desk_x,
            desk_w: dk_w,
            desk_h: dk_h,
            plus_x,
            plus_w: 14.0 * scale,
        }
    }

    fn render(&self) -> Pixmap {
        let w = self.win_w.max(1) as u32;
        let h = self.win_h.max(1) as u32;
        let mut pm = match Pixmap::new(w, h) {
            Some(p) => p,
            None => return Pixmap::new(1, 1).unwrap(),
        };

        let mut paint = Paint::default();
        paint.set_color_rgba8(18, 18, 22, TOPBAR_ALPHA);
        if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32) {
            pm.as_mut()
                .fill_rect(r, &paint, Transform::identity(), None);
        }

        let text_y = (self.win_h as f32 - self.font_px) / 2.0;

        // 前两个按钮
        let mut x = self.pad;
        for (i, (label, _)) in self.buttons.iter().enumerate() {
            if i == 2 {
                // 「帮助」放在桌面组之后
                x = self.plus_x + self.plus_w + self.gap;
            }
            let color = if self.hover == Some(i) {
                [255, 255, 255, 255]
            } else {
                [225, 225, 232, 255]
            };
            draw_text(&mut pm, label, x, text_y, self.font_px, color);
            x += text_width(label, self.font_px) + self.gap;
        }

        // 桌面缩略图组：三张层叠的小卡片，最上面那张代表当前桌面
        let step = 5.0 * (self.desk_h / 10.0);
        let cy = (self.win_h as f32 - self.desk_h) / 2.0;
        let hover_desk = self.hover == Some(3);
        let hover_plus = self.hover == Some(4);
        for k in 0..3 {
            let off = k as f32 * step;
            let px = self.desk_x + off;
            let py = cy + off * 0.5 - k as f32 * 1.0;
            let block = rounded_rect(px, py, self.desk_w, self.desk_h, 2.0);
            let mut bp = Paint::default();
            let a = if hover_desk { 255 } else { 205 };
            bp.set_color_rgba8(235, 235, 245, a);
            bp.anti_alias = true;
            pm.as_mut()
                .fill_path(&block, &bp, FillRule::Winding, Transform::identity(), None);
        }
        // 最上面一张描个深色边，看起来是"当前"
        let front = rounded_rect(self.desk_x + step * 2.0, cy + step - 2.0, self.desk_w, self.desk_h, 2.0);
        let mut sp = Paint::default();
        sp.set_color_rgba8(40, 40, 60, 220);
        sp.anti_alias = true;
        pm.as_mut().fill_path(
            &front,
            &sp,
            FillRule::Winding,
            Transform::identity(),
            None,
        );

        // 加号（两条细矩形拼成十字）
        let plus_cy = self.win_h as f32 / 2.0;
        let mut pp = Paint::default();
        let a = if hover_plus { 255 } else { 210 };
        pp.set_color_rgba8(235, 235, 245, a);
        pp.anti_alias = true;
        let half = 4.0 * (self.desk_h / 10.0);
        let cx = self.plus_x + self.plus_w / 2.0;
        if let Some(v) =
            tiny_skia::Rect::from_xywh(cx - half * 0.12, plus_cy - half, half * 0.24, half * 2.0)
        {
            pm.as_mut()
                .fill_rect(v, &pp, Transform::identity(), None);
        }
        if let Some(h) =
            tiny_skia::Rect::from_xywh(cx - half, plus_cy - half * 0.12, half * 2.0, half * 0.24)
        {
            pm.as_mut()
                .fill_rect(h, &pp, Transform::identity(), None);
        }

        // 右侧：时间 / 电量 / 音量，从右往左排
        let t = now_hhmm();
        let tw = text_width(&t, self.font_px);
        let mut right = self.win_w as f32 - self.pad - tw;
        draw_text(&mut pm, &t, right, text_y, self.font_px, [225, 225, 232, 255]);

        if let Some(b) = unsafe { battery_percent() } {
            let charging = unsafe { battery_charging() };
            let s = if charging {
                format!("{}%+", b)
            } else {
                format!("{}%", b)
            };
            let bw = text_width(&s, self.font_px);
            right -= self.gap + bw;
            let color = if !charging && b <= 20 {
                [255, 150, 140, 255]
            } else {
                [200, 200, 210, 255]
            };
            draw_text(&mut pm, &s, right, text_y, self.font_px, color);
        }

        if let Some(v) = unsafe { system_volume() } {
            let muted = unsafe { is_muted() };
            let s = if muted {
                "静音".to_string()
            } else {
                format!("{}%", v)
            };
            let vw = text_width(&s, self.font_px);
            right -= self.gap + vw;
            let color = if muted {
                [255, 150, 140, 255]
            } else {
                [200, 200, 210, 255]
            };
            draw_text(&mut pm, &s, right, text_y, self.font_px, color);
        }

        pm
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        if y < 0.0 || y > self.win_h as f32 || x < 0.0 || x > self.win_w as f32 {
            return None;
        }
        // 桌面缩略图组
        if x >= self.desk_x - 4.0 && x <= self.desk_x + self.desk_w + 14.0 {
            return Some(3);
        }
        // 加号
        if x >= self.plus_x - 3.0 && x <= self.plus_x + self.plus_w + 3.0 {
            return Some(4);
        }
        // 文字按钮
        let mut cx = self.pad;
        for (i, (label, _)) in self.buttons.iter().enumerate() {
            if i == 2 {
                cx = self.plus_x + self.plus_w + self.gap;
            }
            let w = text_width(label, self.font_px);
            if x >= cx - self.gap * 0.5 && x <= cx + w + self.gap * 0.5 {
                return Some(i);
            }
            cx += w + self.gap;
        }
        // 音量文字
        if let Some(vx) = self.volume_x() {
            let w = unsafe {
                if system_volume().is_some() {
                    text_width(
                        &if is_muted() {
                            "静音".to_string()
                        } else {
                            format!("{}%", system_volume().unwrap_or(0))
                        },
                        self.font_px,
                    )
                } else {
                    0.0
                }
            };
            if x >= vx - 6.0 && x <= vx + w + 6.0 {
                return Some(5);
            }
        }
        None
    }

    fn action_of(&self, idx: usize) -> Act {
        match idx {
            0 => Act::NewFile,
            1 => Act::NewFolder,
            2 => Act::Help,
            3 => Act::ShowDesktops,
            4 => Act::NewDesktop,
            _ => Act::ToggleMute,
        }
    }

    /// 右侧音量文字的左边界，点击静音
    fn volume_x(&self) -> Option<f32> {
        let t = now_hhmm();
        let tw = text_width(&t, self.font_px);
        let mut right = self.win_w as f32 - self.pad - tw;
        unsafe {
            if let Some(b) = battery_percent() {
                let s = if battery_charging() {
                    format!("{}%+", b)
                } else {
                    format!("{}%", b)
                };
                right -= self.gap + text_width(&s, self.font_px);
            }
            if let Some(v) = system_volume() {
                let s = if is_muted() {
                    "静音".to_string()
                } else {
                    format!("{}%", v)
                };
                let vw = text_width(&s, self.font_px);
                return Some(right - self.gap - vw);
            }
        }
        None
    }
}

static mut TOPBAR: Option<TopBar> = None;

unsafe fn redraw_topbar(hwnd: HWND) {
    if let Some(tb) = TOPBAR.as_ref() {
        let pm = tb.render();
        present(hwnd, &pm);
    }
}

/// 弹系统「另存为」对话框拿路径。filters 为空时当成选文件夹名用
unsafe fn pick_save_path(
    title: &str,
    def_ext: &str,
    filters: &[(&str, &str)],
    overwrite: bool,
) -> Option<PathBuf> {
    use winapi::um::commdlg::{
        GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_NOVALIDATE, OFN_OVERWRITEPROMPT,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let mut file_buf = [0u16; 512];
    let mut filter: Vec<u16> = Vec::new();
    for (name, pat) in filters {
        filter.extend(name.encode_utf16());
        filter.push(0);
        filter.extend(pat.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    if filters.is_empty() {
        filter.push(0);
    }

    let title_w = wide(title);
    let ext_w = wide(def_ext);

    let mut ofn: OPENFILENAMEW = zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as DWORD;
    ofn.lpstrFilter = if filters.is_empty() {
        null()
    } else {
        filter.as_ptr()
    };
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as DWORD;
    ofn.lpstrTitle = title_w.as_ptr();
    let mut flags = OFN_NOCHANGEDIR | OFN_NOVALIDATE | OFN_PATHMUSTEXIST;
    if overwrite {
        flags |= OFN_OVERWRITEPROMPT;
    }
    ofn.Flags = flags;
    if !def_ext.is_empty() {
        ofn.lpstrDefExt = ext_w.as_ptr();
    }

    if GetSaveFileNameW(&mut ofn) == 0 {
        return None;
    }
    let len = file_buf.iter().position(|&c| c == 0).unwrap_or(0);
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&file_buf[..len])))
}

fn action_new_file() {
    let filters = [
        ("文本文档 (*.txt)", "*.txt"),
        ("Markdown (*.md)", "*.md"),
        ("Word 文档 (*.docx)", "*.docx"),
        ("Excel 表格 (*.xlsx)", "*.xlsx"),
        ("所有文件 (*.*)", "*.*"),
    ];
    unsafe {
        if let Some(p) = pick_save_path("新建文件", "txt", &filters, true) {
            match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&p)
            {
                Ok(_) => {
                    debug_log(&format!("new_file: {}", p.display()));
                    launch(&p.display().to_string());
                }
                Err(e) => debug_log(&format!("new_file 失败: {} ({})", p.display(), e)),
            }
        }
    }
}

fn action_new_folder() {
    unsafe {
        if let Some(p) = pick_save_path("新建文件夹", "", &[], false) {
            match std::fs::create_dir_all(&p) {
                Ok(_) => {
                    debug_log(&format!("new_folder: {}", p.display()));
                    launch(&p.display().to_string());
                }
                Err(e) => debug_log(&format!("new_folder 失败: {} ({})", p.display(), e)),
            }
        }
    }
}

/// 模拟 Win+Ctrl+D 新建虚拟桌面
unsafe fn new_virtual_desktop() {
    use winapi::um::winuser::{keybd_event, KEYEVENTF_KEYUP, VK_CONTROL, VK_LWIN};

    keybd_event(VK_LWIN as u8, 0, 0, 0);
    keybd_event(VK_CONTROL as u8, 0, 0, 0);
    keybd_event(b'D', 0, 0, 0);
    keybd_event(b'D', 0, KEYEVENTF_KEYUP, 0);
    keybd_event(VK_CONTROL as u8, 0, KEYEVENTF_KEYUP, 0);
    keybd_event(VK_LWIN as u8, 0, KEYEVENTF_KEYUP, 0);
}

/// 电池剩余百分比。没有电池（台式机）或读数未知时返回 None
unsafe fn battery_percent() -> Option<u8> {
    use winapi::um::winbase::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut st: SYSTEM_POWER_STATUS = zeroed();
    if GetSystemPowerStatus(&mut st) == 0 {
        return None;
    }
    if st.BatteryLifePercent == 255 {
        return None;
    }
    Some(st.BatteryLifePercent)
}

/// 是否接着电源
unsafe fn battery_charging() -> bool {
    use winapi::um::winbase::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut st: SYSTEM_POWER_STATUS = zeroed();
    if GetSystemPowerStatus(&mut st) == 0 {
        return false;
    }
    st.ACLineStatus == 1
}

/// 模拟 Win+Tab 打开任务视图，多个虚拟桌面在那里并排可选
unsafe fn show_task_view() {
    use winapi::um::winuser::{keybd_event, KEYEVENTF_KEYUP, VK_LWIN, VK_TAB};

    keybd_event(VK_LWIN as u8, 0, 0, 0);
    keybd_event(VK_TAB as u8, 0, 0, 0);
    keybd_event(VK_TAB as u8, 0, KEYEVENTF_KEYUP, 0);
    keybd_event(VK_LWIN as u8, 0, KEYEVENTF_KEYUP, 0);
}

fn do_action(act: Act) {
    match act {
        Act::Help => launch("https://github.com/changfengpolang191/RustDock/issues"),
        Act::NewFile => action_new_file(),
        Act::NewFolder => action_new_folder(),
        Act::ShowDesktops => unsafe { show_task_view() },
        Act::NewDesktop => unsafe { new_virtual_desktop() },
        Act::ToggleMute => unsafe { toggle_mute() },
    }
}

unsafe extern "system" fn topbar_wndproc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => 0,
        // 时间每分钟变，用定时器定期重绘；顺便刷新运行中应用集合
        WM_TIMER => {
            refresh_running();
            redraw_topbar(hwnd);
            if !DOCK_HWND.is_null() {
                redraw(DOCK_HWND);
            }
            0
        }
        WM_MOUSEMOVE => {
            let mut tme: TRACKMOUSEEVENT = zeroed();
            tme.cbSize = size_of::<TRACKMOUSEEVENT>() as u32;
            tme.dwFlags = TME_LEAVE;
            tme.hwndTrack = hwnd;
            TrackMouseEvent(&mut tme);

            if let Some(tb) = TOPBAR.as_mut() {
                let h = tb.hit(px(lparam), py(lparam));
                if h != tb.hover {
                    tb.hover = h;
                    redraw_topbar(hwnd);
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            if let Some(tb) = TOPBAR.as_mut() {
                if tb.hover.is_some() {
                    tb.hover = None;
                    redraw_topbar(hwnd);
                }
            }
            0
        }
        WM_LBUTTONUP => {
            let act = match TOPBAR.as_ref() {
                Some(tb) => tb.hit(px(lparam), py(lparam)),
                None => None,
            };
            if let Some(i) = act {
                let a = TOPBAR.as_ref().map(|tb| tb.action_of(i));
                if let Some(a) = a {
                    do_action(a);
                    redraw_topbar(hwnd);
                }
            }
            0
        }
        // 滚轮调音量：WM_MOUSEWHEEL 给的是屏幕坐标
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16;
            let mut pt = POINT {
                x: px(lparam) as i32,
                y: py(lparam) as i32,
            };
            ScreenToClient(hwnd, &mut pt);
            let on_volume = match TOPBAR.as_ref() {
                Some(tb) => tb.hit(pt.x as f32, pt.y as f32) == Some(5),
                None => false,
            };
            if on_volume {
                let step = if delta > 0 { 0.05 } else { -0.05 };
                adjust_volume(step);
                redraw_topbar(hwnd);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    // --install-autostart / --uninstall-autostart 只改注册表，不开界面
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--install-autostart") {
        std::process::exit(if unsafe { set_autostart(true) } { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--uninstall-autostart") {
        std::process::exit(if unsafe { set_autostart(false) } { 0 } else { 1 });
    }
    // 任务栏被移到屏幕外后 Dock 异常退出，用这个参数把它挪回来
    if args.iter().any(|a| a == "--restore-taskbar") {
        unsafe {
            let tray = taskbar_window();
            if !tray.is_null() {
                let sh = GetSystemMetrics(SM_CYSCREEN);
                SetWindowPos(
                    tray,
                    null_mut(),
                    0,
                    sh - 48,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
        std::process::exit(0);
    }

    unsafe {
        SetProcessDPIAware(); // 不声明就是 DPI 虚化，150% 缩放下会很糊

        let dc = GetDC(null_mut());
        let dpi = GetDeviceCaps(dc, LOGPIXELSX) as f32;
        ReleaseDC(null_mut(), dc);
        let scale = (dpi / 96.0).max(1.0);

        DOCK = Some(Dock::new(load_items(), scale));
        let (win_w, win_h) = match DOCK.as_ref() {
            Some(d) => (d.win_w, d.win_h),
            None => return,
        };

        let hinst = GetModuleHandleW(null());
        // explorer 重启后会广播这个消息
        TASKBAR_CREATED_MSG = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let class: Vec<u16> = OsStr::new("RustDockWnd")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let title: Vec<u16> = OsStr::new("RustDock").encode_wide().chain(Some(0)).collect();

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            // winapi 里 IDC_ARROW 是 LPWSTR(*mut u16)，LoadCursorW 要 LPCWSTR(*const u16)
            hCursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);

        // LAYERED: 能用 UpdateLayeredWindow；TOOLWINDOW: 不进任务栏/Alt-Tab；
        // TOPMOST: 一直在最前；NOACTIVATE: 点它不抢走前台窗口焦点
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            win_w,
            win_h,
            null_mut(),
            null_mut(),
            hinst,
            null_mut(),
        );
        if hwnd.is_null() {
            return;
        }

        // Dock 始终按任务栏停靠位置让出空隙
        let base_y = dock_base_y(scale);
        let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
        let y = (base_y - win_h).max(0);

        hide_taskbar();

        SetWindowPos(
            hwnd,
            // 桌面层：普通窗口能盖住它。没被遮挡时它就是这个位置最上层的窗口，
            // 鼠标自然能命中；被遮挡时说明用户看不见它，也不该抢上来压住应用。
            HWND_BOTTOM,
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        // 先收集运行中应用，再画第一帧（指示点依赖这个集合）
        DOCK_HWND = hwnd;
        refresh_running();
        redraw(hwnd);

        // ── 顶部栏 ──
        TOPBAR = Some(TopBar::new(scale));
        let (tb_w, tb_h) = match TOPBAR.as_ref() {
            Some(t) => (t.win_w, t.win_h),
            None => (0, 0),
        };
        let tb_class = wide("RustDockTopBar");
        let tb_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(topbar_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: tb_class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&tb_wc);
        let tb = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            tb_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            tb_w,
            tb_h,
            null_mut(),
            null_mut(),
            hinst,
            null_mut(),
        );
        if !tb.is_null() {
            SetWindowPos(
                tb,
                HWND_TOPMOST,
                0,
                0,
                tb_w,
                tb_h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            redraw_topbar(tb);
            SetTimer(tb, 1, 1000, None);
        }

        // 桌面图标由 Dock 接管，退出时还回去
        if set_desktop_icons_hidden(true) {
            HID_DESKTOP_ICONS = true;
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
