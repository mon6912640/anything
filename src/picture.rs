//! 图片解码（预览面板用），全部依赖系统自带组件，无需安装任何东西：
//! - PNG/JPG/BMP/GIF/TIFF/ICO/WebP/DDS/HEIC 等：Windows 图像组件 WIC
//! - TGA、PSD/PSB：自己解码（PSD 读取文件里的合并图像，不需要 Photoshop）
//! - SVG：Direct2D 渲染
//! - 以上失败或其他格式：借用资源管理器的缩略图（装了对应缩略图扩展时可用）
//!
//! 输出预乘 alpha 的 BGRA。大图缩小到不超过给定框（高质量插值），小图保持原尺寸。
//! 调用线程需先调用 `com_init()`。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use windows::Win32::Foundation::{GENERIC_READ, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{D2D_SIZE_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SHCreateMemStream, SIIGBF_THUMBNAILONLY};
use windows::core::{HSTRING, Interface, PCWSTR, w};

use crate::util::long_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 位图：src_w/src_h 是原图尺寸
    Raster,
    /// 矢量图（SVG），可以按任意尺寸重新渲染
    Vector,
    /// 资源管理器缩略图，不知道原图尺寸
    Thumb,
}

pub struct Picture {
    pub w: u32,
    pub h: u32,
    /// 预乘 alpha 的 BGRA，每行 w*4 字节，自上而下
    pub bgra: Vec<u8>,
    pub src_w: u32,
    pub src_h: u32,
    /// 格式名，如 "PNG"
    pub format: String,
    /// 补充信息，如 "32 位"、"3 帧"
    pub detail: String,
    /// 含有透明像素
    pub alpha: bool,
    pub kind: Kind,
    /// 解码时使用的框大小
    pub box_w: u32,
    pub box_h: u32,
}

/// 预览面板会尝试解码的扩展名
pub const EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "bmp", "dib", "gif", "tif", "tiff", "ico", "cur", "webp", "dds", "tga", "psd", "psb", "svg", "heic",
    "heif", "avif", "jxr", "wdp", "hdp",
];

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(p) if !name[p..].contains('\\') => name[p + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn is_image(name: &str) -> bool {
    let e = ext_of(name);
    EXTS.contains(&e.as_str())
}

/// 把 sw×sh 缩放到框 bw×bh 内（保持比例）。upscale=false 时小图不放大。
pub fn fit(sw: u32, sh: u32, bw: u32, bh: u32, upscale: bool) -> (u32, u32) {
    if sw == 0 || sh == 0 || bw == 0 || bh == 0 {
        return (sw.max(1), sh.max(1));
    }
    if !upscale && sw <= bw && sh <= bh {
        return (sw, sh);
    }
    let s = (bw as f64 / sw as f64).min(bh as f64 / sh as f64);
    (((sw as f64 * s).round() as u32).max(1), ((sh as f64 * s).round() as u32).max(1))
}

pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

pub fn load(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let bw = bw.max(1);
    let bh = bh.max(1);
    let r = match ext_of(path).as_str() {
        "tga" => load_tga(path, bw, bh),
        "psd" | "psb" => load_psd(path, bw, bh),
        "svg" => load_svg(path, bw, bh),
        // 未压缩、BC4/BC5 自己解（WIC 对灰度等格式解得不对），BC1~BC3（DXT1~5）交给 WIC
        "dds" => load_dds(path, bw, bh).or_else(|e| load_wic(path, bw, bh).map_err(|_| e)),
        _ => load_wic(path, bw, bh),
    };
    let mut p = match r {
        Ok(p) => p,
        Err(e) => load_thumb(path, bw, bh).map_err(|_| e)?,
    };
    p.box_w = bw;
    p.box_h = bh;
    Ok(p)
}

fn describe(e: &windows::core::Error) -> String {
    match e.code().0 as u32 {
        0x88982F50 | 0x88982F07 | 0x88982F8B => "没有这种格式的解码器（可安装对应的 Windows 图像扩展）".into(),
        0x80070002 | 0x80070003 => "文件不存在".into(),
        0x80070005 => "没有权限读取文件".into(),
        0x80070020 => "文件被其他程序占用".into(),
        0x88982F60 | 0x88982F61 | 0x88982F62 => "文件已损坏或格式不正确".into(),
        c => format!("解码失败（0x{c:08X}）"),
    }
}

fn wic() -> Result<IWICImagingFactory, String> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(|e| describe(&e)) }
}

/// 把任意 WIC 位图源转成预乘 BGRA，并缩小到框内
fn finish(f: &IWICImagingFactory, src: &IWICBitmapSource, sw: u32, sh: u32, bw: u32, bh: u32) -> windows::core::Result<Picture> {
    unsafe {
        let (w, h) = fit(sw, sh, bw, bh, false);
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;
        // 在预乘空间里缩放，透明边缘不会发黑
        let out: IWICBitmapSource = if (w, h) != (sw, sh) {
            let s = f.CreateBitmapScaler()?;
            s.Initialize(&conv, w, h, WICBitmapInterpolationModeHighQualityCubic)?;
            s.cast()?
        } else {
            conv.cast()?
        };
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        out.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
        // 三次插值在硬边缘会过冲，预乘颜色超过 alpha 时 GDI 混合会溢出成杂色
        let mut alpha = false;
        for p in buf.chunks_exact_mut(4) {
            let a = p[3];
            if a != 255 {
                alpha = true;
                p[0] = p[0].min(a);
                p[1] = p[1].min(a);
                p[2] = p[2].min(a);
            }
        }
        Ok(Picture { w, h, bgra: buf, src_w: sw, src_h: sh, format: String::new(), detail: String::new(), alpha, kind: Kind::Raster, box_w: bw, box_h: bh })
    }
}

fn container_name(g: &windows::core::GUID) -> &'static str {
    let names = [
        (GUID_ContainerFormatPng, "PNG"),
        (GUID_ContainerFormatJpeg, "JPEG"),
        (GUID_ContainerFormatBmp, "BMP"),
        (GUID_ContainerFormatGif, "GIF"),
        (GUID_ContainerFormatTiff, "TIFF"),
        (GUID_ContainerFormatIco, "ICO"),
        (GUID_ContainerFormatDds, "DDS"),
        (GUID_ContainerFormatWebp, "WebP"),
        (GUID_ContainerFormatHeif, "HEIF"),
        (GUID_ContainerFormatWmp, "JPEG XR"),
        (GUID_ContainerFormatRaw, "RAW"),
    ];
    names.iter().find(|(id, _)| id == g).map(|(_, n)| *n).unwrap_or("")
}

fn load_wic(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let f = wic()?;
    // 长路径前缀只能加在绝对路径上
    let b = path.as_bytes();
    let lp = if (b.len() > 2 && b[1] == b':') || path.starts_with(r"\\") { long_path(path) } else { crate::util::wide(path) };
    let r = (|| unsafe {
        let dec = f.CreateDecoderFromFilename(PCWSTR(lp.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let fmt = dec.GetContainerFormat()?;
        let n = dec.GetFrameCount()?.max(1);
        let ico = fmt == GUID_ContainerFormatIco;
        // ICO 里有多个尺寸，取最大的一张
        let mut idx = 0;
        if ico && n > 1 {
            let mut best = 0u64;
            for i in 0..n {
                let fr = dec.GetFrame(i)?;
                let (mut w, mut h) = (0, 0);
                fr.GetSize(&mut w, &mut h)?;
                if (w as u64 * h as u64) > best {
                    best = w as u64 * h as u64;
                    idx = i;
                }
            }
        }
        let frame = dec.GetFrame(idx)?;
        let (mut sw, mut sh) = (0, 0);
        frame.GetSize(&mut sw, &mut sh)?;
        let pf = frame.GetPixelFormat()?;
        let bits = f
            .CreateComponentInfo(&pf)
            .and_then(|c| c.cast::<IWICPixelFormatInfo>())
            .and_then(|i| i.GetBitsPerPixel())
            .unwrap_or(0);
        let mut p = finish(&f, &frame.cast()?, sw, sh, bw, bh)?;
        p.format = match container_name(&fmt) {
            "" => ext_of(path).to_ascii_uppercase(),
            s => s.to_string(),
        };
        let mut d = Vec::new();
        if bits > 0 {
            d.push(format!("{bits} 位"));
        }
        if n > 1 {
            d.push(if ico { format!("{n} 个尺寸") } else { format!("{n} 帧") });
        }
        p.detail = d.join(" · ");
        Ok(p)
    })();
    r.map_err(|e: windows::core::Error| describe(&e))
}

/// 自己解码出来的非预乘 BGRA 交给 WIC 统一做预乘和缩放
fn from_straight(w: u32, h: u32, bgra: &[u8], bw: u32, bh: u32) -> Result<Picture, String> {
    let f = wic()?;
    unsafe {
        let bmp = f.CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppBGRA, w * 4, bgra).map_err(|e| describe(&e))?;
        finish(&f, &bmp.cast().map_err(|e| describe(&e))?, w, h, bw, bh).map_err(|e| describe(&e))
    }
}

// ---------------------------------------------------------------- 资源管理器缩略图

fn load_thumb(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    unsafe {
        let fac: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None).map_err(|e| describe(&e))?;
        let hbm = fac.GetImage(SIZE { cx: bw as i32, cy: bh as i32 }, SIIGBF_THUMBNAILONLY).map_err(|e| describe(&e))?;
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(HGDIOBJ(hbm.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
        let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.unsigned_abs());
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        };
        let dc = GetDC(None);
        let got = GetDIBits(dc, hbm, 0, h, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        if got == 0 || w == 0 || h == 0 {
            return Err("无法获取缩略图".into());
        }
        if buf.chunks_exact(4).all(|p| p[3] == 0) {
            // 没有 alpha 通道
            for p in buf.chunks_exact_mut(4) {
                p[3] = 255;
            }
        } else if buf.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
            // 非预乘，自己乘上
            premultiply(&mut buf);
        }
        let alpha = buf.chunks_exact(4).any(|p| p[3] != 255);
        Ok(Picture {
            w,
            h,
            bgra: buf,
            src_w: 0,
            src_h: 0,
            format: ext_of(path).to_ascii_uppercase(),
            detail: "资源管理器缩略图".into(),
            alpha,
            kind: Kind::Thumb,
            box_w: bw,
            box_h: bh,
        })
    }
}

fn premultiply(buf: &mut [u8]) {
    for p in buf.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

// ---------------------------------------------------------------- SVG（Direct2D）

fn load_svg(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let data = std::fs::read(path).map_err(|e| format!("无法读取文件：{e}"))?;
    let f = wic()?;
    let r = (|| unsafe {
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = |w: u32, h: u32| -> windows::core::Result<(IWICBitmap, ID2D1RenderTarget, ID2D1DeviceContext5)> {
            let bmp = f.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
            let rt = d2d.CreateWicBitmapRenderTarget(&bmp, &props)?;
            let dc: ID2D1DeviceContext5 = rt.cast()?;
            Ok((bmp, rt, dc))
        };
        let doc = |dc: &ID2D1DeviceContext5, w: f32, h: f32| -> windows::core::Result<ID2D1SvgDocument> {
            let stream = SHCreateMemStream(Some(&data)).ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x8007000Eu32 as i32)))?;
            dc.CreateSvgDocument(&stream, D2D_SIZE_F { width: w, height: h })
        };

        // 第一遍：取原始尺寸（width/height，没有或是百分比时用 viewBox）
        let (_, _, dc0) = target(1, 1)?;
        let root = doc(&dc0, 1.0, 1.0)?.GetRoot()?;
        let mut vb = D2D1_SVG_VIEWBOX::default();
        let has_vb = root.IsAttributeSpecified(w!("viewBox"), None).as_bool()
            && root
                .GetAttributeValue2(w!("viewBox"), D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX, &mut vb as *mut _ as *mut _, std::mem::size_of::<D2D1_SVG_VIEWBOX>() as u32)
                .is_ok()
            && vb.width > 0.0
            && vb.height > 0.0;
        let len = |name: PCWSTR| -> Option<f32> {
            if !root.IsAttributeSpecified(name, None).as_bool() {
                return None;
            }
            let mut l = D2D1_SVG_LENGTH::default();
            root.GetAttributeValue2(name, D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH, &mut l as *mut _ as *mut _, std::mem::size_of::<D2D1_SVG_LENGTH>() as u32).ok()?;
            (l.units == D2D1_SVG_LENGTH_UNITS_NUMBER && l.value > 0.0).then_some(l.value)
        };
        let (lw, lh) = (len(w!("width")), len(w!("height")));
        let (iw, ih) = match (lw, lh) {
            (Some(a), Some(b)) => (a, b),
            _ if has_vb => (vb.width, vb.height),
            (Some(a), None) => (a, a),
            (None, Some(b)) => (b, b),
            _ => (300.0, 150.0),
        };
        let (sw, sh) = (iw.ceil().clamp(1.0, 65535.0) as u32, ih.ceil().clamp(1.0, 65535.0) as u32);

        // 第二遍：按框大小渲染（矢量图放大也清晰）
        let (w, h) = fit(sw, sh, bw, bh, true);
        let (bmp, rt, dc) = target(w, h)?;
        let d = doc(&dc, w as f32, h as f32)?;
        let root = d.GetRoot()?;
        if !has_vb {
            let v = D2D1_SVG_VIEWBOX { x: 0.0, y: 0.0, width: iw, height: ih };
            root.SetAttributeValue2(w!("viewBox"), D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX, &v as *const _ as *const _, std::mem::size_of::<D2D1_SVG_VIEWBOX>() as u32)?;
        }
        let full = D2D1_SVG_LENGTH { value: 100.0, units: D2D1_SVG_LENGTH_UNITS_PERCENTAGE };
        for name in [w!("width"), w!("height")] {
            root.SetAttributeValue2(name, D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH, &full as *const _ as *const _, std::mem::size_of::<D2D1_SVG_LENGTH>() as u32)?;
        }
        rt.BeginDraw();
        rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
        dc.DrawSvgDocument(&d);
        rt.EndDraw(None, None)?;
        drop(dc);
        drop(rt);
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        bmp.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
        let alpha = buf.chunks_exact(4).any(|p| p[3] != 255);
        Ok(Picture { w, h, bgra: buf, src_w: sw, src_h: sh, format: "SVG".into(), detail: "矢量图".into(), alpha, kind: Kind::Vector, box_w: bw, box_h: bh })
    })();
    r.map_err(|e: windows::core::Error| match e.code().0 as u32 {
        0x80004002 => "系统版本过低，不支持 SVG".to_string(),
        _ => "SVG 解析失败（Direct2D 只支持 SVG 的常用子集）".to_string(),
    })
}

// ---------------------------------------------------------------- TGA

fn load_tga(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let data = std::fs::read(path).map_err(|e| format!("无法读取文件：{e}"))?;
    let t = decode_tga(&data)?;
    let mut p = from_straight(t.w, t.h, &t.bgra, bw, bh)?;
    p.format = "TGA".into();
    p.detail = t.detail;
    Ok(p)
}

pub struct Decoded {
    pub w: u32,
    pub h: u32,
    /// 非预乘 BGRA
    pub bgra: Vec<u8>,
    pub detail: String,
}

fn u16le(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

pub fn decode_tga(d: &[u8]) -> Result<Decoded, String> {
    let bad = || "TGA 文件已损坏或格式不支持".to_string();
    if d.len() < 18 {
        return Err(bad());
    }
    let (idlen, cmtype, itype) = (d[0] as usize, d[1], d[2]);
    let (cmfirst, cmlen, cmbits) = (u16le(d, 3) as usize, u16le(d, 5) as usize, d[7]);
    let (w, h, bpp, desc) = (u16le(d, 12) as usize, u16le(d, 14) as usize, d[16], d[17]);
    let base = itype & 7;
    let rle = itype & 8 != 0;
    if w == 0 || h == 0 || !matches!(base, 1..=3) || !matches!(bpp, 8 | 15 | 16 | 24 | 32) || (base == 1 && cmtype != 1) {
        return Err(bad());
    }
    let alpha_bits = desc & 15;
    let mut pos = 18 + idlen;

    // 15/16 位色转 BGRA
    let c16 = |v: u16, with_alpha: bool| -> [u8; 4] {
        let ex = |x: u16| ((x << 3) | (x >> 2)) as u8;
        let a = if with_alpha { if v & 0x8000 != 0 { 255 } else { 0 } } else { 255 };
        [ex(v & 31), ex((v >> 5) & 31), ex((v >> 10) & 31), a]
    };
    let mut pal: Vec<[u8; 4]> = Vec::new();
    if cmtype == 1 {
        let eb = (cmbits as usize).div_ceil(8);
        let need = cmlen * eb;
        if eb == 0 || pos + need > d.len() {
            return Err(bad());
        }
        for i in 0..cmlen {
            let e = &d[pos + i * eb..pos + (i + 1) * eb];
            pal.push(match eb {
                2 => c16(u16le(e, 0), cmbits == 16 && alpha_bits > 0),
                3 => [e[0], e[1], e[2], 255],
                4 => [e[0], e[1], e[2], e[3]],
                _ => [e[0], e[0], e[0], 255],
            });
        }
        pos += need;
    }
    let pb = (bpp as usize).div_ceil(8);
    let px = |s: &[u8]| -> [u8; 4] {
        match base {
            1 => {
                let i = if pb == 1 { s[0] as usize } else { u16le(s, 0) as usize };
                pal.get(i.wrapping_sub(cmfirst)).copied().unwrap_or([0, 0, 0, 255])
            }
            3 => {
                if pb == 2 {
                    [s[0], s[0], s[0], s[1]]
                } else {
                    [s[0], s[0], s[0], 255]
                }
            }
            _ => match pb {
                2 => c16(u16le(s, 0), bpp == 16 && alpha_bits > 0),
                3 => [s[0], s[1], s[2], 255],
                _ => [s[0], s[1], s[2], s[3]],
            },
        }
    };
    let n = w * h;
    if n > 1 << 26 {
        return Err("图片太大".into());
    }
    let mut out = vec![0u8; n * 4];
    let mut k = 0;
    if rle {
        while k < n {
            let c = *d.get(pos).ok_or_else(bad)?;
            pos += 1;
            let cnt = (c as usize & 0x7f) + 1;
            if c & 0x80 != 0 {
                let s = d.get(pos..pos + pb).ok_or_else(bad)?;
                pos += pb;
                let v = px(s);
                for _ in 0..cnt.min(n - k) {
                    out[k * 4..k * 4 + 4].copy_from_slice(&v);
                    k += 1;
                }
            } else {
                for _ in 0..cnt.min(n - k) {
                    let s = d.get(pos..pos + pb).ok_or_else(bad)?;
                    pos += pb;
                    out[k * 4..k * 4 + 4].copy_from_slice(&px(s));
                    k += 1;
                }
            }
        }
    } else {
        let s = d.get(pos..pos + n * pb).ok_or_else(bad)?;
        for (i, c) in s.chunks_exact(pb).enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&px(c));
        }
    }
    // 默认自下而上存放；bit4 = 从右到左
    let row = w * 4;
    if desc & 0x20 == 0 {
        for y in 0..h / 2 {
            let (a, b) = out.split_at_mut((h - 1 - y) * row);
            a[y * row..(y + 1) * row].swap_with_slice(&mut b[..row]);
        }
    }
    if desc & 0x10 != 0 {
        for r in out.chunks_exact_mut(row) {
            for x in 0..w / 2 {
                for c in 0..4 {
                    r.swap(x * 4 + c, (w - 1 - x) * 4 + c);
                }
            }
        }
    }
    // 不少 32 位 TGA 的 alpha 全是 0（实际不透明）
    if out.chunks_exact(4).all(|p| p[3] == 0) {
        for p in out.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    let mut detail = format!("{bpp} 位");
    if rle {
        detail.push_str(" · RLE");
    }
    Ok(Decoded { w: w as u32, h: h as u32, bgra: out, detail })
}

// ---------------------------------------------------------------- DDS（WIC 不支持的部分）

fn load_dds(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let data = std::fs::read(path).map_err(|e| format!("无法读取文件：{e}"))?;
    let t = decode_dds(&data)?;
    let mut p = from_straight(t.w, t.h, &t.bgra, bw, bh)?;
    p.format = "DDS".into();
    p.detail = t.detail;
    Ok(p)
}

fn u32le(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i..i + 4].try_into().unwrap())
}

/// BC4 块（8 字节）解出 16 个值
fn bc4_block(b: &[u8]) -> [u8; 16] {
    let (r0, r1) = (b[0] as u32, b[1] as u32);
    let mut pal = [0u8; 8];
    pal[0] = r0 as u8;
    pal[1] = r1 as u8;
    if r0 > r1 {
        for i in 1..7u32 {
            pal[i as usize + 1] = (((7 - i) * r0 + i * r1 + 3) / 7) as u8;
        }
    } else {
        for i in 1..5u32 {
            pal[i as usize + 1] = (((5 - i) * r0 + i * r1 + 2) / 5) as u8;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let mut bits = 0u64;
    for i in 0..6 {
        bits |= (b[2 + i] as u64) << (8 * i);
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = pal[((bits >> (3 * i)) & 7) as usize];
    }
    out
}

pub fn decode_dds(d: &[u8]) -> Result<Decoded, String> {
    let bad = || "DDS 文件已损坏或格式不支持".to_string();
    if d.len() < 128 || &d[0..4] != b"DDS " || u32le(d, 4) != 124 {
        return Err(bad());
    }
    let (h, w) = (u32le(d, 12) as usize, u32le(d, 16) as usize);
    let (pf_flags, fourcc) = (u32le(d, 80), &d[84..88]);
    let bits = u32le(d, 88);
    // 有的工具把 8/16 位格式的掩码写在高位，挪回来
    let fix = |m: u32| if bits < 32 && bits > 0 && m >> bits != 0 { m >> (32 - bits) } else { m };
    let masks = [fix(u32le(d, 92)), fix(u32le(d, 96)), fix(u32le(d, 100)), fix(u32le(d, 104))];
    if w == 0 || h == 0 || w * h > 1 << 26 {
        return Err(bad());
    }
    enum F {
        Masks,
        Rgba8 { bgr: bool, alpha: bool },
        Bc4,
        Bc5,
    }
    let mut pos = 128;
    let (fmt, name) = if pf_flags & 4 != 0 {
        match fourcc {
            b"ATI1" | b"BC4U" => (F::Bc4, "BC4".to_string()),
            b"ATI2" | b"BC5U" => (F::Bc5, "BC5".to_string()),
            b"DX10" => {
                if d.len() < 148 {
                    return Err(bad());
                }
                pos = 148;
                match u32le(d, 128) {
                    28 | 29 => (F::Rgba8 { bgr: false, alpha: true }, "RGBA8".to_string()),
                    87 | 91 => (F::Rgba8 { bgr: true, alpha: true }, "BGRA8".to_string()),
                    88 | 93 => (F::Rgba8 { bgr: true, alpha: false }, "BGRX8".to_string()),
                    79 | 80 => (F::Bc4, "BC4".to_string()),
                    82 | 83 => (F::Bc5, "BC5".to_string()),
                    70..=78 => return Err("BC1~BC3".into()),
                    95..=99 => return Err("BC6H/BC7 压缩的 DDS 暂不支持".into()),
                    f => return Err(format!("不支持的 DDS 像素格式（DXGI {f}）")),
                }
            }
            f => return Err(format!("不支持的 DDS 压缩格式（{}）", String::from_utf8_lossy(f))),
        }
    } else if matches!(bits, 8 | 16 | 24 | 32) {
        (F::Masks, format!("{bits} 位"))
    } else {
        return Err(bad());
    };

    let mut out = vec![0u8; w * h * 4];
    match fmt {
        F::Masks => {
            let pb = bits as usize / 8;
            let src = d.get(pos..pos + w * h * pb).ok_or_else(bad)?;
            let lum = pf_flags & 0x20000 != 0;
            let alpha_only = pf_flags & 2 != 0 && pf_flags & 0x40 == 0 && !lum;
            let has_a = pf_flags & 3 != 0 && masks[3] != 0;
            let ch = |v: u32, m: u32| -> u8 {
                if m == 0 {
                    return 0;
                }
                let sh = m.trailing_zeros();
                let max = (m >> sh) as u64;
                (((v & m) >> sh) as u64 * 255 / max) as u8
            };
            for (i, px) in src.chunks_exact(pb).enumerate() {
                let mut v = 0u32;
                for (k, b) in px.iter().enumerate() {
                    v |= (*b as u32) << (8 * k);
                }
                let a = if has_a { ch(v, masks[3]) } else { 255 };
                let o = &mut out[i * 4..i * 4 + 4];
                if alpha_only {
                    o.copy_from_slice(&[0, 0, 0, ch(v, masks[3])]);
                } else if lum {
                    let l = ch(v, masks[0]);
                    o.copy_from_slice(&[l, l, l, a]);
                } else {
                    o.copy_from_slice(&[ch(v, masks[2]), ch(v, masks[1]), ch(v, masks[0]), a]);
                }
            }
        }
        F::Rgba8 { bgr, alpha } => {
            let src = d.get(pos..pos + w * h * 4).ok_or_else(bad)?;
            for (i, px) in src.chunks_exact(4).enumerate() {
                let (b, r) = if bgr { (px[0], px[2]) } else { (px[2], px[0]) };
                out[i * 4..i * 4 + 4].copy_from_slice(&[b, px[1], r, if alpha { px[3] } else { 255 }]);
            }
        }
        F::Bc4 | F::Bc5 => {
            let two = matches!(fmt, F::Bc5);
            let bs = if two { 16 } else { 8 };
            let (bx, by) = (w.div_ceil(4), h.div_ceil(4));
            let src = d.get(pos..pos + bx * by * bs).ok_or_else(bad)?;
            for (k, blk) in src.chunks_exact(bs).enumerate() {
                let (x0, y0) = ((k % bx) * 4, (k / bx) * 4);
                let r = bc4_block(&blk[..8]);
                let g = if two { bc4_block(&blk[8..16]) } else { r };
                for j in 0..16 {
                    let (x, y) = (x0 + j % 4, y0 + j / 4);
                    if x < w && y < h {
                        // BC4 显示为灰度，BC5（常用于法线贴图）显示 R、G 两个通道
                        let px = if two { [0, g[j], r[j], 255] } else { [r[j], r[j], r[j], 255] };
                        out[(y * w + x) * 4..][..4].copy_from_slice(&px);
                    }
                }
            }
        }
    }
    Ok(Decoded { w: w as u32, h: h as u32, bgra: out, detail: name })
}

// ---------------------------------------------------------------- PSD / PSB

fn load_psd(path: &str, bw: u32, bh: u32) -> Result<Picture, String> {
    let mut file = File::open(path).map_err(|e| format!("无法读取文件：{e}"))?;
    // 大图边解码边按整数倍平均缩小，内存只和预览尺寸有关
    let limit = (bw.max(bh) * 2).max(1024);
    let (t, sw, sh, psb) = decode_psd(&mut file, limit)?;
    let mut p = from_straight(t.w, t.h, &t.bgra, bw, bh)?;
    p.src_w = sw;
    p.src_h = sh;
    p.format = if psb { "PSB" } else { "PSD" }.into();
    p.detail = t.detail;
    Ok(p)
}

struct Rd<'a> {
    f: &'a mut File,
}

impl Rd<'_> {
    fn buf(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut b = vec![0u8; n];
        self.f.read_exact(&mut b).map_err(|_| "PSD 文件不完整".to_string())?;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.buf(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.buf(4)?;
        Ok(u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        let b = self.buf(8)?;
        Ok(u64::from_be_bytes(b.try_into().unwrap()))
    }
    fn len(&mut self, psb: bool) -> Result<u64, String> {
        if psb { self.u64() } else { self.u32().map(|v| v as u64) }
    }
    fn pos(&mut self) -> Result<u64, String> {
        self.f.stream_position().map_err(|e| e.to_string())
    }
    fn seek(&mut self, p: u64) -> Result<(), String> {
        self.f.seek(SeekFrom::Start(p)).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// PackBits 解压一行
fn unpack_bits(src: &[u8], dst: &mut [u8]) {
    let (mut i, mut o) = (0, 0);
    while i < src.len() && o < dst.len() {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let c = (n as usize + 1).min(src.len() - i).min(dst.len() - o);
            dst[o..o + c].copy_from_slice(&src[i..i + c]);
            i += n as usize + 1;
            o += c;
        } else if n != -128 {
            let c = (1 - n as isize) as usize;
            let v = src.get(i).copied().unwrap_or(0);
            i += 1;
            let c = c.min(dst.len() - o);
            dst[o..o + c].fill(v);
            o += c;
        }
    }
}

/// 返回 (缩小后的图, 原宽, 原高, 是否 PSB)
pub fn decode_psd(f: &mut File, limit: u32) -> Result<(Decoded, u32, u32, bool), String> {
    let mut r = Rd { f };
    let bad = || "PSD 文件已损坏或格式不支持".to_string();
    if r.buf(4)? != b"8BPS" {
        return Err(bad());
    }
    let ver = r.u16()?;
    if ver != 1 && ver != 2 {
        return Err(bad());
    }
    let psb = ver == 2;
    r.buf(6)?;
    let channels = r.u16()? as usize;
    let h = r.u32()? as usize;
    let w = r.u32()? as usize;
    let depth = r.u16()?;
    let mode = r.u16()?;
    if w == 0 || h == 0 || channels == 0 || !matches!(depth, 8 | 16 | 32) {
        return Err("不支持的 PSD 位深".into());
    }
    let mode_name = match mode {
        1 => "灰度",
        2 => "索引色",
        3 => "RGB",
        4 => "CMYK",
        _ => return Err("不支持的 PSD 颜色模式（仅支持 RGB/CMYK/灰度/索引色）".into()),
    };
    // 颜色模式数据（索引色的调色板）
    let cm_len = r.u32()? as usize;
    let cm = r.buf(cm_len)?;
    if mode == 2 && cm.len() < 768 {
        return Err(bad());
    }
    // 图像资源
    let ir_len = r.u32()? as u64;
    let p = r.pos()?;
    r.seek(p + ir_len)?;
    // 图层与蒙版：图层数为负表示第一个额外通道是合并图像的透明度
    let lm_len = r.len(psb)?;
    let lm_start = r.pos()?;
    let mut merged_alpha = false;
    if lm_len >= if psb { 10 } else { 6 } {
        let li_len = r.len(psb)?;
        if li_len >= 2 {
            merged_alpha = (r.u16()? as i16) < 0;
        }
    }
    r.seek(lm_start + lm_len)?;

    let comp = r.u16()?;
    if comp > 1 {
        return Err("不支持的 PSD 压缩方式".into());
    }
    let (color_ch, alpha_ch) = match mode {
        1 => (1, 1),
        2 => (1, usize::MAX),
        3 => (3, 3),
        _ => (4, 4),
    };
    if channels < color_ch {
        return Err(bad());
    }
    let has_alpha = merged_alpha && alpha_ch < channels;
    let used = if has_alpha { color_ch + 1 } else { color_ch };

    let bps = depth as usize / 8;
    let row_bytes = w * bps;
    let data_start;
    let mut row_len: Vec<u64> = Vec::new();
    if comp == 1 {
        let cnt = channels * h;
        let b = r.buf(cnt * if psb { 4 } else { 2 })?;
        row_len = if psb {
            b.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap()) as u64).collect()
        } else {
            b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as u64).collect()
        };
        data_start = r.pos()?;
    } else {
        data_start = r.pos()?;
    }
    // 每个通道数据的起点
    let mut ch_start = vec![data_start; channels + 1];
    for c in 0..channels {
        let sz = if comp == 1 { row_len[c * h..(c + 1) * h].iter().sum::<u64>() } else { (row_bytes * h) as u64 };
        ch_start[c + 1] = ch_start[c] + sz;
    }

    let s = (w.max(h) as u32).div_ceil(limit.max(1)).max(1) as usize;
    let (w2, h2) = (w.div_ceil(s), h.div_ceil(s));
    if (w2 * h2) > 1 << 26 {
        return Err("图片太大".into());
    }
    // 索引色按调色板展开成 3 个通道再累加
    let acc_ch = if mode == 2 { 3 } else { used };
    let mut acc = vec![0u32; acc_ch * w2 * h2];
    let mut raw = Vec::new();
    let mut row = vec![0u8; row_bytes];
    let sample = |row: &[u8], x: usize| -> u32 {
        match bps {
            1 => row[x] as u32,
            2 => row[x * 2] as u32,
            _ => {
                let v = f32::from_be_bytes(row[x * 4..x * 4 + 4].try_into().unwrap());
                (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32
            }
        }
    };
    for c in 0..used {
        let src_c = if c < color_ch { c } else { alpha_ch };
        r.seek(ch_start[src_c])?;
        for y in 0..h {
            if comp == 1 {
                let n = row_len[src_c * h + y] as usize;
                raw.resize(n, 0);
                r.f.read_exact(&mut raw).map_err(|_| "PSD 文件不完整".to_string())?;
                row.fill(0);
                unpack_bits(&raw, &mut row);
            } else {
                r.f.read_exact(&mut row).map_err(|_| "PSD 文件不完整".to_string())?;
            }
            let oy = (y / s) * w2;
            if mode == 2 {
                for x in 0..w {
                    let i = sample(&row, x) as usize;
                    let o = oy + x / s;
                    for k in 0..3 {
                        acc[k * w2 * h2 + o] += cm[k * 256 + i] as u32;
                    }
                }
            } else {
                let base = c * w2 * h2 + oy;
                for x in 0..w {
                    acc[base + x / s] += sample(&row, x);
                }
            }
        }
    }

    let plane = w2 * h2;
    let mut out = vec![0u8; plane * 4];
    for oy in 0..h2 {
        let ch = (h - oy * s).min(s) as u32;
        for ox in 0..w2 {
            let cw = (w - ox * s).min(s) as u32;
            let n = cw * ch;
            let i = oy * w2 + ox;
            let v = |c: usize| ((acc[c * plane + i] + n / 2) / n) as u8;
            let (rr, gg, bb) = match mode {
                1 => (v(0), v(0), v(0)),
                2 | 3 => (v(0), v(1), v(2)),
                _ => {
                    // PSD 的 CMYK 是反向存储的（255 = 无墨）
                    let k = v(3) as u32;
                    let m = |x: u8| ((x as u32 * k + 127) / 255) as u8;
                    (m(v(0)), m(v(1)), m(v(2)))
                }
            };
            let a = if has_alpha { v(color_ch) } else { 255 };
            out[i * 4..i * 4 + 4].copy_from_slice(&[bb, gg, rr, a]);
        }
    }
    let detail = format!("{mode_name} {depth} 位");
    Ok((Decoded { w: w2 as u32, h: h2 as u32, bgra: out, detail }, w as u32, h as u32, psb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_rules() {
        assert_eq!(fit(100, 50, 400, 400, false), (100, 50));
        assert_eq!(fit(800, 400, 400, 400, false), (400, 200));
        assert_eq!(fit(10, 20, 400, 400, true), (200, 400));
    }

    #[test]
    fn tga_rle_bottom_up() {
        // 2x2，24 位 RLE，自下而上：底行红红，顶行绿蓝
        let mut d = vec![0u8; 18];
        d[2] = 10;
        d[12] = 2;
        d[14] = 2;
        d[16] = 24;
        d.extend_from_slice(&[0x81, 0, 0, 255]); // 2 个红（BGR）
        d.extend_from_slice(&[0x01, 0, 255, 0, 255, 0, 0]); // 绿、蓝
        let t = decode_tga(&d).unwrap();
        assert_eq!((t.w, t.h), (2, 2));
        assert_eq!(&t.bgra[0..4], &[0, 255, 0, 255]); // 左上 绿
        assert_eq!(&t.bgra[4..8], &[255, 0, 0, 255]); // 右上 蓝
        assert_eq!(&t.bgra[8..12], &[0, 0, 255, 255]); // 左下 红
    }

    #[test]
    fn packbits() {
        let mut out = [0u8; 6];
        unpack_bits(&[2, 1, 2, 3, 0xFE, 9], &mut out);
        assert_eq!(out, [1, 2, 3, 9, 9, 9]);
    }
}
