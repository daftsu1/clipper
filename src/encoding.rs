// Elección del codificador de vídeo según la GPU, y los argumentos de ffmpeg de cada familia:
//   - NVENC (NVIDIA): captura y exportación directas en GPU. Es lo probado a fondo.
//   - AMF (AMD): EXPERIMENTAL. No se ha podido probar en una GPU AMD; los fotogramas se bajan a memoria
//     (hwdownload) porque la ruta directa d3d11 → AMF tiene fallos conocidos.
//   - CPU (libx264): funciona en cualquier PC pero usa bastante más procesador.
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

use crate::{silent_cmd, Config};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Nvenc,
    Amf,
    Cpu,
}

pub fn family(enc: &str) -> Family {
    if enc.ends_with("_nvenc") {
        Family::Nvenc
    } else if enc.ends_with("_amf") {
        Family::Amf
    } else {
        Family::Cpu
    }
}

/// Codificador por hardware (si falla, se puede caer a CPU).
pub fn is_hardware(enc: &str) -> bool {
    family(enc) != Family::Cpu
}

/// Elige el codificador según los nombres de las GPU del equipo: NVIDIA > AMD > CPU.
/// Ignora adaptadores virtuales (emuladores, escritorio remoto) y los básicos de Windows.
pub fn pick_for_gpus(names: &[String]) -> &'static str {
    let reales: Vec<String> = names
        .iter()
        .map(|n| n.to_ascii_lowercase())
        .filter(|n| !["virtual", "basic", "remote", "mirror", "indirect", "parsec", "citrix"].iter().any(|w| n.contains(w)))
        .collect();
    if reales.iter().any(|n| n.contains("nvidia")) {
        "h264_nvenc"
    } else if reales.iter().any(|n| n.contains("amd") || n.contains("radeon") || n.contains("advanced micro")) {
        "h264_amf"
    } else {
        "libx264"
    }
}

fn gpu_names() -> Vec<String> {
    let out = silent_cmd(Path::new("powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-Command", "Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }"])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Detección (una sola vez por ejecución) del codificador adecuado y las GPU vistas, para el registro.
fn detect() -> &'static (String, Vec<String>) {
    static D: OnceLock<(String, Vec<String>)> = OnceLock::new();
    D.get_or_init(|| {
        let names = gpu_names();
        (pick_for_gpus(&names).to_string(), names)
    })
}

/// Codificador real: «auto» se resuelve según la GPU; cualquier otro valor se respeta.
pub fn resolve(cfg_encoder: &str) -> String {
    if cfg_encoder == "auto" {
        detect().0.clone()
    } else {
        cfg_encoder.to_string()
    }
}

/// Descripción de la detección para el registro (solo tiene sentido con «auto»).
pub fn detection_note() -> String {
    let (enc, names) = detect();
    format!("GPU detectada: {} → codificador {enc}", if names.is_empty() { "ninguna".into() } else { names.join(" + ") })
}

/// Argumentos de vídeo de la grabación continua (incluye el filtro de bajada a memoria cuando hace falta).
pub fn capture_args(enc: &str, cfg: &Config, gop: u32) -> Vec<String> {
    let br = format!("{}M", cfg.bitrate_mbps);
    let mr = format!("{}M", cfg.maxrate_mbps);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let mut a: Vec<String> = Vec::new();
    match family(enc) {
        Family::Nvenc => {
            // Sin B-frames ni anticipación: NVENC reserva muchas menos superficies (≈85 MB menos de RAM
            // en ffmpeg, medido) sin perder fotogramas; la compresión es algo menos eficiente.
            a.extend(s(&["-c:v", enc, "-preset", &cfg.nvenc_preset, "-rc", "vbr", "-b:v", &br, "-maxrate", &mr, "-bf", "0", "-rc-lookahead", "0"]));
        }
        Family::Amf => {
            // ddagrab entrega fotogramas dentro de la GPU (d3d11): se bajan a memoria. forced_idr hace que cada
            // fotograma clave sea IDR, imprescindible para cortar el búfer en segmentos independientes.
            a.extend(s(&["-vf", "hwdownload,format=bgra,format=nv12"]));
            a.extend(s(&["-c:v", enc, "-quality", "balanced", "-rc", "vbr_peak", "-b:v", &br, "-maxrate", &mr, "-bf", "0", "-forced_idr", "1"]));
        }
        Family::Cpu => {
            // Un codificador de CPU no puede leer fotogramas d3d11: hay que bajarlos a memoria.
            a.extend(s(&["-vf", "hwdownload,format=bgra,format=yuv420p"]));
            a.extend(s(&["-c:v", "libx264", "-preset", "veryfast", "-b:v", &br, "-maxrate", &mr, "-bufsize", &mr]));
        }
    }
    a.extend(["-g".to_string(), gop.to_string()]);
    a
}

/// Argumentos de codificación para el recorte «Exacto» (H.264 de buena calidad, compatible en todas partes).
pub fn exact_args(fam: Family) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    match fam {
        Family::Nvenc => s(&["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", "21", "-b:v", "0", "-maxrate", "30M"]),
        Family::Amf => s(&["-c:v", "h264_amf", "-quality", "quality", "-rc", "vbr_peak", "-b:v", "15M", "-maxrate", "30M"]),
        Family::Cpu => s(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "21", "-maxrate", "30M", "-bufsize", "60M"]),
    }
}

/// Argumentos de codificación para «Para Discord» con un bitrate de vídeo objetivo en kbps.
pub fn discord_args(fam: Family, vk: u32) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let (k, buf) = (format!("{vk}k"), format!("{}k", vk * 2));
    let mut a = match fam {
        Family::Nvenc => s(&["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr"]),
        Family::Amf => s(&["-c:v", "h264_amf", "-quality", "quality", "-rc", "vbr_peak"]),
        Family::Cpu => s(&["-c:v", "libx264", "-preset", "veryfast"]),
    };
    a.extend(["-b:v".to_string(), k.clone(), "-maxrate".into(), k, "-bufsize".into(), buf]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn elige_codificador_segun_la_gpu() {
        assert_eq!(pick_for_gpus(&v(&["NVIDIA GeForce RTX 5060", "MuMuPlayer Virtual Display Adapter"])), "h264_nvenc");
        assert_eq!(pick_for_gpus(&v(&["AMD Radeon RX 6700 XT"])), "h264_amf");
        assert_eq!(pick_for_gpus(&v(&["AMD Radeon(TM) Graphics"])), "h264_amf");
        assert_eq!(pick_for_gpus(&v(&["Intel(R) UHD Graphics", "NVIDIA GeForce GTX 1650"])), "h264_nvenc", "portátil híbrido: NVIDIA primero");
        assert_eq!(pick_for_gpus(&v(&["Intel(R) UHD Graphics"])), "libx264", "Intel sin probar: CPU");
        assert_eq!(pick_for_gpus(&v(&["Microsoft Basic Display Adapter"])), "libx264");
        assert_eq!(pick_for_gpus(&v(&["NVIDIA Virtual Display (Parsec)"])), "libx264", "los virtuales no cuentan");
        assert_eq!(pick_for_gpus(&[]), "libx264", "sin información: CPU");
    }

    #[test]
    fn familias() {
        assert_eq!(family("h264_nvenc"), Family::Nvenc);
        assert_eq!(family("av1_nvenc"), Family::Nvenc);
        assert_eq!(family("h264_amf"), Family::Amf);
        assert_eq!(family("libx264"), Family::Cpu);
        assert!(is_hardware("hevc_amf") && is_hardware("h264_nvenc") && !is_hardware("libx264"));
    }

    #[test]
    fn captura_baja_los_fotogramas_solo_si_no_es_nvenc() {
        let cfg = Config::default();
        let n = capture_args("h264_nvenc", &cfg, 120).join(" ");
        assert!(!n.contains("hwdownload") && n.contains("-bf 0 -rc-lookahead 0") && n.ends_with("-g 120"), "{n}");
        let a = capture_args("h264_amf", &cfg, 120).join(" ");
        assert!(a.contains("hwdownload,format=bgra,format=nv12") && a.contains("-forced_idr 1") && a.contains("-rc vbr_peak"), "{a}");
        let c = capture_args("libx264", &cfg, 120).join(" ");
        assert!(c.contains("hwdownload,format=bgra,format=yuv420p") && c.contains("-c:v libx264"), "{c}");
    }

    #[test]
    fn exportacion_por_familia() {
        assert!(exact_args(Family::Nvenc).join(" ").contains("h264_nvenc"));
        assert!(exact_args(Family::Amf).join(" ").contains("h264_amf"));
        assert!(exact_args(Family::Cpu).join(" ").contains("-crf 21"));
        let d = discord_args(Family::Amf, 900).join(" ");
        assert!(d.contains("h264_amf") && d.contains("-b:v 900k -maxrate 900k -bufsize 1800k"), "{d}");
        assert!(discord_args(Family::Cpu, 900).join(" ").contains("libx264"));
    }
}
