/// 应用图标提取
///
/// 从启动目标（`.exe` / `.lnk` / `.url` 等）取出 Windows 系统图标，
/// 转成 gpui 可直接渲染的位图，让列表里每个应用都显示自己的真实图标。
/// 取不到图标时返回 `None`，由调用方回退到首字母色块。
///
/// 像素格式方面，gpui 的约定是 `RenderImage` 的数据为 **BGRA** 字节
/// （gpui 自己解码图片时也会把 RGBA 逐像素交换成 BGRA 再交给渲染器）。
/// Windows 用 `GetDIBits` 读 32bpp 位图得到的正好就是 BGRA，因此无需再转换。
///
/// 应用**自身**的图标不走这里：它由 `build.rs` 嵌进 exe 的资源段，
/// 托盘需要时直接用 `LoadImageW` 从资源里取（见 `crate::tray`）。
use std::path::Path;
use std::sync::{Arc, Mutex};

use gpui::RenderImage;

/// 缩放后图标的最大边长（像素）。
///
/// 列表里的图标显示为 28px，64px 足以顶住 2x 缩放；再大只是白白占用
/// 纹理图集（系统图像列表给出的 jumbo 图标是 256×256）。
const MAX_ICON_EDGE: u32 = 64;

/// 把 shell 图标查询串行化的进程级锁。
///
/// shell 的图标接口并发调用会直接失败：实测多线程同时查询时
/// `SHGetFileInfoW` 返回 0，`SHGetImageList` 报 `E_OUTOFMEMORY`；
/// 串行调用则全部成功。所以不论调用方怎样使用线程池，这里都排好队。
static EXTRACT_LOCK: Mutex<()> = Mutex::new(());

/// 读取指定启动目标的图标。失败（路径不存在、不是可执行文件等）返回 `None`。
pub fn load(target: &Path) -> Option<Arc<RenderImage>> {
    let (width, height, bgra) = {
        let _guard = EXTRACT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        imp::icon_bgra(target)?
    };

    let image = image::RgbaImage::from_raw(width, height, bgra)?;
    let image = downscale(image);
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(image)])))
}

/// 把过大的图标等比缩到 [`MAX_ICON_EDGE`] 以内。
fn downscale(image: image::RgbaImage) -> image::RgbaImage {
    let (width, height) = (image.width(), image.height());
    let longest = width.max(height);
    if longest <= MAX_ICON_EDGE {
        return image;
    }

    let scale = MAX_ICON_EDGE as f64 / longest as f64;
    let target_width = ((width as f64 * scale).round() as u32).max(1);
    let target_height = ((height as f64 * scale).round() as u32).max(1);
    resample_box(&image, target_width, target_height)
}

/// 盒式滤波重采样到精确尺寸。
///
/// 用盒式滤波（每个目标像素取源区域的平均值）而不是 `imageops::resize`：
/// 系统给的 jumbo 图标是 256×256，Lanczos 在 debug 构建下单个图标要几十毫秒，
/// 足以卡住列表首帧。盒式滤波只扫一遍源像素。
///
/// 两点实现要点：
/// - 按**预乘 alpha** 求平均。若直接平均通道值，透明区域的黑像素会掺进
///   半透明边缘，让图标边缘发灰发暗。
/// - **不关心通道含义**，原样保留通道顺序（BGRA 进就 BGRA 出，RGBA 进就 RGBA 出）。
///
/// 用于缩小；放大时退化成最近邻（各目标像素只取到一个源像素）。
fn resample_box(
    source_image: &image::RgbaImage,
    target_width: u32,
    target_height: u32,
) -> image::RgbaImage {
    let (width, height) = (source_image.width(), source_image.height());
    if width == 0 || height == 0 {
        return source_image.clone();
    }

    let mut output = vec![0u8; (target_width * target_height * 4) as usize];
    {
        let source = source_image.as_raw();
        for target_y in 0..target_height {
            let y_start = (target_y as u64 * height as u64 / target_height as u64) as u32;
            let y_end = (((target_y as u64 + 1) * height as u64 / target_height as u64) as u32)
                .max(y_start + 1)
                .min(height);

            for target_x in 0..target_width {
                let x_start = (target_x as u64 * width as u64 / target_width as u64) as u32;
                let x_end = (((target_x as u64 + 1) * width as u64 / target_width as u64) as u32)
                    .max(x_start + 1)
                    .min(width);

                let mut sums = [0u32; 4];
                let mut samples = 0u32;
                for y in y_start..y_end {
                    let row = y as usize * width as usize * 4;
                    for x in x_start..x_end {
                        let pixel = &source[row + x as usize * 4..][..4];
                        let alpha = pixel[3] as u32;
                        sums[0] += pixel[0] as u32 * alpha;
                        sums[1] += pixel[1] as u32 * alpha;
                        sums[2] += pixel[2] as u32 * alpha;
                        sums[3] += alpha;
                        samples += 1;
                    }
                }

                let alpha_sum = sums[3];
                let offset = (target_y * target_width + target_x) as usize * 4;
                output[offset + 3] = (alpha_sum / samples) as u8;
                // 完全透明的目标像素没有有效颜色，`checked_div` 会让它保持 0
                for channel in 0..3 {
                    output[offset + channel] =
                        sums[channel].checked_div(alpha_sum).unwrap_or(0) as u8;
                }
            }
        }
    }

    match image::RgbaImage::from_raw(target_width, target_height, output) {
        Some(resampled) => resampled,
        None => source_image.clone(),
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        BITMAP, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
        GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Controls::{ILD_TRANSPARENT, IImageList};
    use windows::Win32::UI::Shell::{
        SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SYSICONINDEX, SHGetFileInfoW, SHGetImageList,
        SHIL_JUMBO,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    /// 取出图标的像素数据，返回 `(宽, 高, BGRA 字节)`。
    pub(super) fn icon_bgra(target: &Path) -> Option<(u32, u32, Vec<u8>)> {
        ensure_com_initialized();
        let path = wide_path(target);

        // 优先用系统图像列表里的 jumbo 图标（256×256，画质最好）；
        // 特殊条目取不到时退回 Shell 自带的大图标（32×32）。
        let hicon = unsafe { jumbo_icon(&path) }.or_else(|| unsafe { shell_large_icon(&path) })?;

        let pixels = unsafe { hicon_to_bgra(hicon) };
        unsafe {
            let _ = DestroyIcon(hicon);
        }
        pixels
    }

    /// `SHGetFileInfo` / `SHGetImageList` 要求 COM 已初始化。
    ///
    /// 必须是**每线程**一次：图标在后台线程池上提取，池里每个线程各有自己的
    /// COM 单元，进程级 OnceLock 只能保证第一个线程初始化过。
    ///
    /// 用 STA（`COINIT_APARTMENTTHREADED`）与资源管理器保持一致，这样依赖
    /// 单元线程模型的第三方图标处理程序也能正常工作。线程池线程不会跑消息
    /// 循环，所以调用必须串行（见 [`super::EXTRACT_LOCK`]）。
    fn ensure_com_initialized() {
        thread_local! {
            static COM: std::cell::OnceCell<()> = const { std::cell::OnceCell::new() };
        }

        COM.with(|cell| {
            cell.get_or_init(|| unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            });
        });
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// 查系统图像列表，取该路径对应的 jumbo 图标。
    unsafe fn jumbo_icon(path: &[u16]) -> Option<HICON> {
        let index = unsafe { icon_index(path) }?;
        let list: IImageList = unsafe { SHGetImageList(SHIL_JUMBO as i32) }.ok()?;
        unsafe { list.GetIcon(index, ILD_TRANSPARENT.0) }.ok()
    }

    /// 用 `SHFILEINFOW` 查询文件，拿它在系统图像列表中的图标序号。
    unsafe fn icon_index(path: &[u16]) -> Option<i32> {
        let mut info = SHFILEINFOW::default();
        let ok = unsafe {
            SHGetFileInfoW(
                PCWSTR(path.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX,
            )
        };
        (ok != 0).then_some(info.iIcon)
    }

    /// 退回 `SHGetFileInfo` 自带的大图标（32×32）。
    unsafe fn shell_large_icon(path: &[u16]) -> Option<HICON> {
        let mut info = SHFILEINFOW::default();
        let ok = unsafe {
            SHGetFileInfoW(
                PCWSTR(path.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        (ok != 0 && !info.hIcon.0.is_null()).then_some(info.hIcon)
    }

    /// `GetIconInfo` 借出的两张位图，离开作用域时释放，避免泄漏 GDI 对象。
    struct IconBitmaps {
        color: HBITMAP,
        mask: HBITMAP,
    }

    impl Drop for IconBitmaps {
        fn drop(&mut self) {
            unsafe {
                if !self.color.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(self.color.0));
                }
                if !self.mask.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(self.mask.0));
                }
            }
        }
    }

    /// 把 HICON 拆成像素：取彩色位图的 32bpp BGRA，必要时用掩码补 alpha。
    unsafe fn hicon_to_bgra(hicon: HICON) -> Option<(u32, u32, Vec<u8>)> {
        let mut info = ICONINFO::default();
        unsafe { GetIconInfo(hicon, &mut info) }.ok()?;
        let bitmaps = IconBitmaps {
            color: info.hbmColor,
            mask: info.hbmMask,
        };

        let (width, height) = unsafe { bitmap_size(bitmaps.color) }?;
        let mut pixels = unsafe { dib_bits(bitmaps.color, width, height) }?;

        // 现代图标自带 alpha 通道；老式图标 alpha 全为 0，透明度在掩码位图里。
        let has_alpha = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0);
        if !has_alpha {
            let mask = match unsafe { bitmap_size(bitmaps.mask) } {
                Some(size) if size == (width, height) => {
                    unsafe { dib_bits(bitmaps.mask, width, height) }
                }
                _ => None,
            };
            match mask {
                // 掩码里白色（0xFF）表示透明
                Some(mask) => {
                    for (pixel, mask_pixel) in pixels
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(mask.as_chunks::<4>().0)
                    {
                        pixel[3] = if mask_pixel[0] == 0 { 255 } else { 0 };
                    }
                }
                None => {
                    for pixel in pixels.as_chunks_mut::<4>().0 {
                        pixel[3] = 255;
                    }
                }
            }
        }

        Some((width, height, pixels))
    }

    /// 读取 GDI 位图的尺寸（像素）。
    unsafe fn bitmap_size(bitmap: HBITMAP) -> Option<(u32, u32)> {
        if bitmap.0.is_null() {
            return None;
        }
        let mut info = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                bitmap.into(),
                size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(info).cast()),
            )
        };
        (read != 0 && info.bmWidth > 0 && info.bmHeight > 0)
            .then_some((info.bmWidth as u32, info.bmHeight as u32))
    }

    /// 用 `GetDIBits` 把位图读成自上而下、每像素 32 位的 BGRA 缓冲。
    unsafe fn dib_bits(bitmap: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
        if bitmap.0.is_null() {
            return None;
        }

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        info.bmiHeader.biHeight = -(height as i32); // 负高度 = 自上而下
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        let hdc = unsafe { GetDC(None) };
        if hdc.0.is_null() {
            return None;
        }

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let lines = unsafe {
            GetDIBits(
                hdc,
                bitmap,
                0,
                height,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        unsafe { ReleaseDC(None, hdc) };

        (lines != 0).then_some(pixels)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub(super) fn icon_bgra(_target: &Path) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

#[cfg(test)]
mod resample_tests {
    use super::*;

    /// 造一张 `width`×`height` 的图，像素由 `pixel_at` 给出（通道顺序原样使用）。
    fn image(width: u32, height: u32, pixel_at: impl Fn(u32, u32) -> [u8; 4]) -> image::RgbaImage {
        let mut buffer = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                buffer.extend_from_slice(&pixel_at(x, y));
            }
        }
        image::RgbaImage::from_raw(width, height, buffer).expect("缓冲区尺寸应匹配")
    }

    /// 守住预乘 alpha 那条约束。
    ///
    /// 左半是「全透明且通道值为 0」（即透明黑），右半是不透明白，缩到 1×1。
    /// 若把通道值不分权重地直接平均，结果会被透明黑拉成中灰（≈127）；
    /// 按 alpha 加权则应接近纯白。
    #[test]
    fn resample_weights_channels_by_alpha() {
        let source = image(4, 1, |x, _| {
            if x < 2 {
                [0, 0, 0, 0]
            } else {
                [255, 255, 255, 255]
            }
        });

        let scaled = resample_box(&source, 1, 1);
        let pixel = scaled.get_pixel(0, 0);

        assert_eq!(pixel[3], 127, "alpha 应是两半的平均值");
        assert!(
            pixel[0] > 200,
            "通道值应按 alpha 加权后接近纯白，实际 {}（权重算错会掉到 ≈127）",
            pixel[0]
        );
    }

    /// 整张图都透明时不应崩，也不该出现除零。
    #[test]
    fn resample_handles_fully_transparent_input() {
        let source = image(8, 8, |_, _| [0, 0, 0, 0]);
        let scaled = resample_box(&source, 2, 2);

        assert_eq!((scaled.width(), scaled.height()), (2, 2));
        for pixel in scaled.as_raw().as_chunks::<4>().0 {
            assert_eq!(pixel[3], 0, "透明输入的输出也应全透明");
            assert_eq!(&pixel[..3], &[0, 0, 0], "无有效颜色的像素应保持 0");
        }
    }

    /// 通道顺序要原样保留，不得混淆 —— `load` 依赖「BGRA 进就 BGRA 出」。
    #[test]
    fn resample_preserves_channel_order() {
        // 通道值刻意取 B≠G≠R，任何重排都会让断言失败
        let source = image(4, 4, |_, _| [200, 100, 50, 255]);
        let scaled = resample_box(&source, 2, 2);
        let pixel = scaled.get_pixel(0, 0);

        assert_eq!([pixel[0], pixel[1], pixel[2], pixel[3]], [200, 100, 50, 255]);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn targets() -> Vec<&'static str> {
        [
            r"C:\Windows\System32\notepad.exe",
            r"C:\Windows\explorer.exe",
            r"C:\Windows\System32\cmd.exe",
            r"C:\Windows\System32\mspaint.exe",
            r"C:\Windows\System32\calc.exe",
            r"C:\Windows\System32\shell32.dll",
        ]
        .into_iter()
        .filter(|target| Path::new(target).exists())
        .collect()
    }

    /// 图标是在后台线程池上提取的。这个测试用一批新线程并发提取，守住两条约束：
    /// 每线程都要自己初始化 COM，且 shell 查询必须串行（去掉二者中任何一个，
    /// shell 接口都会开始返回失败）。
    #[test]
    fn loads_icons_from_many_threads() {
        let targets = targets();
        assert!(!targets.is_empty(), "找不到任何系统可执行文件用于测试");

        let handles: Vec<_> = targets
            .into_iter()
            .map(|target| {
                std::thread::spawn(move || {
                    let icon = load(Path::new(target));
                    (target, icon.map(|icon| (icon.size(0).width.0, icon.size(0).height.0)))
                })
            })
            .collect();

        for handle in handles {
            let (target, size) = handle.join().expect("线程 panic");
            let (width, height) = size.unwrap_or_else(|| panic!("{target}: 提取图标失败"));
            let (width, height) = (width as u32, height as u32);
            assert!(width > 0 && height > 0, "{target}: 尺寸 {width}x{height}");
            assert!(
                width.max(height) <= MAX_ICON_EDGE,
                "{target}: 未按 {MAX_ICON_EDGE}px 上限缩放，实际 {width}x{height}"
            );
        }
    }
}
