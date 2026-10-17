// Interfaz web de Clipper: servidor HTTP local (solo 127.0.0.1 y con token) que vive dentro del proceso de
// la bandeja y una página única (Inicio, Clips, Recortar, Configuración) que se abre en una ventana de
// Chrome/Edge en modo aplicación. El servidor ve el estado real de la grabación y aplica la configuración
// al instante; el recorte y la compresión los hace ffmpeg aquí.
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::{encoding, load_config, log, resolve_paths, save_clip, silent_cmd, Config, Paths};

const PAGE: &str = include_str!("ui.html");

// ---------- estado compartido con el bucle principal ----------

pub struct Status {
    /// "recording" | "restarting" | "stopped"
    pub state: String,
    pub ram_mb: f32,
    pub buffer_secs: u32,
}

pub static STATUS: Mutex<Status> = Mutex::new(Status { state: String::new(), ram_mb: 0.0, buffer_secs: 0 });
static EVENTS: Mutex<VecDeque<(String, String)>> = Mutex::new(VecDeque::new());
/// La interfaz guardó la configuración: el bucle principal recarga atajo y grabación.
pub static RELOAD: AtomicBool = AtomicBool::new(false);
/// Para el modo de pruebas `--ui`: termina el proceso.
pub static QUIT: AtomicBool = AtomicBool::new(false);

pub fn set_state(state: &str) {
    if let Ok(mut s) = STATUS.lock() {
        s.state = state.to_string();
    }
}

pub fn set_metrics(ram_mb: f32, buffer_secs: u32) {
    if let Ok(mut s) = STATUS.lock() {
        s.ram_mb = ram_mb;
        s.buffer_secs = buffer_secs;
    }
}

/// Anota un evento visible en «Actividad» (solo mensajes que le sirven al usuario).
pub fn push_event(msg: &str) {
    const KEEP: [&str; 10] = [
        "clip guardado",
        "clip exportado",
        "clip eliminado",
        "no se pudo guardar",
        "configuración recargada",
        "grabación atascada",
        "ffmpeg se detuvo",
        "error de audio",
        "no pude iniciar la grabación",
        "=== clipper iniciado",
    ];
    if !KEEP.iter().any(|k| msg.starts_with(k)) {
        return;
    }
    let text = if let Some(rest) = msg.strip_prefix("clip guardado: ") {
        format!("Clip guardado · {}", Path::new(rest).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    } else if let Some(rest) = msg.strip_prefix("clip exportado: ") {
        format!("Clip exportado · {}", rest.trim_matches('"'))
    } else if let Some(rest) = msg.strip_prefix("clip eliminado (a la papelera): ") {
        format!("Clip enviado a la papelera · {rest}")
    } else if msg.starts_with("=== clipper iniciado") {
        "Clipper iniciado".to_string()
    } else {
        let mut c = msg.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    if let Ok(mut q) = EVENTS.lock() {
        q.push_back((chrono::Local::now().format("%H:%M").to_string(), text));
        while q.len() > 30 {
            q.pop_front();
        }
    }
}

/// Memoria de trabajo (MB) de un proceso, si se puede consultar.
pub fn working_set_mb(pid: u32) -> Option<f32> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut c = PROCESS_MEMORY_COUNTERS::default();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = K32GetProcessMemoryInfo(h, &mut c, c.cb).as_bool();
        let _ = CloseHandle(h);
        if ok { Some(c.WorkingSetSize as f32 / 1_048_576.0) } else { None }
    }
}

// ---------- servidor ----------

pub struct Ui {
    pub token: String,
    pub port: u16,
    pub dir: PathBuf,
}

impl Ui {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?t={}", self.port, self.token)
    }
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn make_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut s = String::new();
    for i in 0..2u128 {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos ^ i);
        s.push_str(&format!("{:016x}", h.finish()));
    }
    s
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let clen = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(64 * 1024);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < clen {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let mut query = HashMap::new();
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            query.insert(percent_decode(k), percent_decode(v));
        }
    }
    Some(Req { method, path: percent_decode(&path), query, headers, body })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], extra: &[(&str, String)]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn respond_json(stream: &mut TcpStream, v: &Value) {
    respond(stream, "200 OK", "application/json; charset=utf-8", v.to_string().as_bytes(), &[]);
}

/// Nombre de clip válido: solo letras, números, punto, guion y guion bajo, con extensión de vídeo.
fn valid_name(n: &str) -> bool {
    let ok_chars = !n.is_empty() && n.len() < 120 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    let l = n.to_ascii_lowercase();
    ok_chars && !n.starts_with('.') && (l.ends_with(".mp4") || l.ends_with(".mkv") || l.ends_with(".webm"))
}

struct Ctx {
    cfg: Config,
    paths: Paths,
    dir: PathBuf,
}

fn clip_path(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    let p = ctx.paths.out_dir.join(name);
    if p.is_file() { Some(p) } else { None }
}

fn serve_file(stream: &mut TcpStream, path: &Path, range: Option<&str>) {
    let Ok(mut f) = File::open(path) else {
        return respond(stream, "404 Not Found", "text/plain", b"no existe", &[]);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let l = path.to_string_lossy().to_ascii_lowercase();
    let ctype = if l.ends_with(".mkv") {
        "video/x-matroska"
    } else if l.ends_with(".webm") {
        "video/webm"
    } else if l.ends_with(".jpg") {
        "image/jpeg"
    } else if l.ends_with(".m4a") {
        "audio/mp4"
    } else {
        "video/mp4"
    };
    if len == 0 {
        return respond(stream, "404 Not Found", "text/plain", b"vacio", &[]);
    }
    let (mut start, mut end, mut partial) = (0u64, len - 1, false);
    if let Some(r) = range.and_then(|r| r.strip_prefix("bytes=")) {
        if let Some((a, b)) = r.split_once('-') {
            partial = true;
            if a.is_empty() {
                let n: u64 = b.parse().unwrap_or(0);
                start = len.saturating_sub(n);
            } else {
                start = a.parse().unwrap_or(0);
                if !b.is_empty() {
                    end = b.parse::<u64>().unwrap_or(len - 1).min(len - 1);
                }
            }
        }
    }
    if start > end || start >= len {
        return respond(stream, "416 Range Not Satisfiable", "text/plain", b"rango", &[("Content-Range", format!("bytes */{len}"))]);
    }
    let n = end - start + 1;
    let status = if partial { "206 Partial Content" } else { "200 OK" };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {n}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if partial {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() || f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut remaining = n;
    let mut chunk = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        match f.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                if stream.write_all(&chunk[..k]).is_err() {
                    break; // el navegador cerró la conexión (salto de posición): normal
                }
                remaining -= k as u64;
            }
        }
    }
}

// ---------- ffmpeg: sonda, recortes y miniaturas ----------

struct Probe {
    duration: f64,
    height: u32,
    /// Nombres de las pistas de audio, en orden («Sistema», «Juego»…); «Pista N» si no tienen nombre.
    audio: Vec<String>,
}

fn probe(ff: &Path, file: &Path) -> Result<Probe, String> {
    let out = silent_cmd(ff).args(["-hide_banner", "-i"]).arg(file).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let mut duration = 0.0;
    if let Some(i) = err.find("Duration: ") {
        let t: String = err[i + 10..].chars().take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.').collect();
        let p: Vec<f64> = t.split(':').filter_map(|x| x.parse().ok()).collect();
        if p.len() == 3 {
            duration = p[0] * 3600.0 + p[1] * 60.0 + p[2];
        }
    }
    let mut height = 0;
    for tok in err.split(|c: char| c == ' ' || c == ',' || c == '[' || c == ']') {
        if let Some((w, h)) = tok.split_once('x') {
            if w.len() >= 3 && w.len() <= 5 && h.len() >= 3 && h.len() <= 5 {
                if let (Ok(_), Ok(hh)) = (w.parse::<u32>(), h.parse::<u32>()) {
                    height = hh;
                    break;
                }
            }
        }
    }
    if duration <= 0.0 {
        return Err("no pude leer la duración del clip".into());
    }
    Ok(Probe { duration, height, audio: audio_track_names(&err) })
}

/// Extrae los nombres de las pistas de audio de la salida de `ffmpeg -i` (título o nombre del manejador).
fn audio_track_names(info: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut named: Vec<bool> = Vec::new();
    let mut cur: Option<usize> = None;
    for l in info.lines() {
        let t = l.trim_start();
        if t.starts_with("Stream #") {
            if t.contains("Audio:") {
                names.push(format!("Pista {}", names.len() + 1));
                named.push(false);
                cur = Some(names.len() - 1);
            } else {
                cur = None;
            }
        } else if let Some(i) = cur {
            if let Some((k, v)) = t.split_once(':') {
                let (k, v) = (k.trim(), v.trim());
                if k == "title" && !v.is_empty() {
                    names[i] = v.to_string();
                    named[i] = true;
                } else if k == "handler_name" && !named[i] && !v.is_empty() && !v.eq_ignore_ascii_case("SoundHandler") {
                    names[i] = v.to_string();
                }
            }
        }
    }
    names
}

fn run_ff(ff: &Path, args: &[String]) -> Result<(), String> {
    let out = silent_cmd(ff).args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = err.lines().rev().take(3).collect();
    Err(format!("ffmpeg falló: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | ")))
}

fn unique_path(dir: &Path, stem: &str, suffix: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}{suffix}.mp4"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{stem}{suffix}_{i}.mp4"));
        i += 1;
    }
    p
}

type MetaCache = Mutex<HashMap<String, (u64, u64, f64, Vec<String>)>>;
fn meta_cache() -> &'static MetaCache {
    static C: OnceLock<MetaCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Duración y pistas de audio de un clip, con caché por (tamaño, fecha) para no sondear en cada refresco.
fn clip_meta(ctx: &Ctx, name: &str, path: &Path, size: u64, mtime: u64) -> (f64, Vec<String>) {
    if let Some((s, m, d, a)) = meta_cache().lock().unwrap().get(name) {
        if *s == size && *m == mtime {
            return (*d, a.clone());
        }
    }
    let (d, a) = probe(&ctx.paths.ffmpeg, path).map(|p| (p.duration, p.audio)).unwrap_or((0.0, Vec::new()));
    meta_cache().lock().unwrap().insert(name.to_string(), (size, mtime, d, a.clone()));
    (d, a)
}

/// Envolvente de picos (0 a 1) de una pista de audio, en `n` tramos, para dibujar su forma de onda.
fn peaks(ctx: &Ctx, name: &str, track: usize, n: usize) -> Result<Vec<f32>, String> {
    static C: OnceLock<Mutex<HashMap<String, Vec<f32>>>> = OnceLock::new();
    let src = clip_path(ctx, name).ok_or("clip no válido")?;
    let md = fs::metadata(&src).map_err(|e| e.to_string())?;
    let mt = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let key = format!("{name}|{track}|{n}|{}|{mt}", md.len());
    let cache = C.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return Ok(v.clone());
    }
    let out = silent_cmd(&ctx.paths.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&src)
        .args(["-map", &format!("0:a:{track}"), "-ac", "1", "-ar", "4000", "-f", "s16le", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() || out.stdout.len() < 2 {
        return Err("no pude leer esa pista de audio".into());
    }
    let samples: Vec<i16> = out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let per = samples.len().div_ceil(n).max(1);
    let mut v: Vec<f32> = samples
        .chunks(per)
        .map(|c| (c.iter().map(|s| (*s as i32).unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0 * 1000.0).round() / 1000.0)
        .collect();
    v.resize(n, 0.0);
    cache.lock().unwrap().insert(key, v.clone());
    Ok(v)
}

/// Extrae una pista de audio a un `.m4a` (sin recodificar) para reproducirla por separado en el editor.
fn track_audio_path(ctx: &Ctx, name: &str, i: usize) -> Option<PathBuf> {
    let src = clip_path(ctx, name)?;
    let dir = ctx.dir.join("thumbs");
    let out = dir.join(format!("{name}.a{i}.m4a"));
    let src_m = fs::metadata(&src).ok()?.modified().ok()?;
    if let Ok(m) = fs::metadata(&out).and_then(|m| m.modified()) {
        if m >= src_m {
            return Some(out);
        }
    }
    fs::create_dir_all(&dir).ok()?;
    // Una sola extracción a la vez: el navegador pide varias pistas (y reintenta) en paralelo, y dos ffmpeg
    // escribiendo el mismo archivo lo dejaban corrupto. Al obtener el turno se vuelve a comprobar la caché.
    let _turn = light_lock();
    if let Ok(m) = fs::metadata(&out).and_then(|m| m.modified()) {
        if m >= src_m {
            return Some(out);
        }
    }
    let tmp = dir.join(format!("{name}.a{i}.tmp"));
    let args: Vec<String> = [
        "-hide_banner", "-loglevel", "error", "-y", "-i", &src.to_string_lossy(), "-map", &format!("0:a:{i}"), "-vn",
        "-c", "copy", "-f", "mp4", "-movflags", "+faststart", &tmp.to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    run_ff(&ctx.paths.ffmpeg, &args).ok()?;
    fs::rename(&tmp, &out).ok()?;
    out.is_file().then_some(out)
}

/// Turno para las tareas ligeras de ffmpeg (miniaturas y pistas de audio): de una en una, para que abrir
/// «Clips» con muchos clips nuevos no lance decenas de ffmpeg a la vez.
fn light_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// Audio de salida según el mezclador del editor: (argumentos de ffmpeg, descripción para el mensaje).
/// «full» = la pista Sistema tal cual; «custom» = suma de las pistas elegidas con su volumen (sin la de Sistema).
fn audio_out_args(req: &Value, ntracks: usize, kbps: u32) -> (Vec<String>, String) {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    if ntracks == 0 {
        return (s(&["-an"]), "sin audio".into());
    }
    if req["audio_mode"].as_str() != Some("custom") || ntracks == 1 {
        // Mezcla completa = pista Sistema + micrófono: el micrófono no sale por el altavoz, así que la
        // captura del sistema nunca lo incluye. Su volumen/silencio del mezclador se respeta.
        let mic = req["mic_index"].as_u64().map(|m| m as usize).filter(|m| *m > 0 && *m < ntracks);
        if let Some(m) = mic {
            let t = req["tracks"].as_array().and_then(|v| v.iter().find(|t| t["index"].as_u64() == Some(m as u64)).cloned());
            let muted = t.as_ref().and_then(|t| t["mute"].as_bool()).unwrap_or(false);
            let g = t.as_ref().and_then(|t| t["gain"].as_f64()).unwrap_or(1.0).clamp(0.0, 1.5);
            if !muted && g > 0.0 {
                let mix = "amix=inputs=2:normalize=0:duration=longest,alimiter=limit=0.95[aout]";
                let graph = if (g - 1.0).abs() > 1e-6 {
                    format!("[0:a:{m}]volume={g:.3}[mv];[0:a:0][mv]{mix}")
                } else {
                    format!("[0:a:0][0:a:{m}]{mix}")
                };
                let mut a = vec!["-filter_complex".to_string(), graph, "-map".to_string(), "[aout]".to_string()];
                a.extend(s(&["-c:a", "aac", "-ac", "2", "-b:a"]));
                a.push(format!("{kbps}k"));
                return (a, "mezcla completa (Sistema + micrófono)".into());
            }
        }
        let mut a = s(&["-map", "0:a:0", "-c:a", "aac", "-ac", "2", "-b:a"]);
        a.push(format!("{kbps}k"));
        return (a, if ntracks > 1 { "mezcla completa (pista Sistema)".into() } else { "audio original".into() });
    }
    let mut sel: Vec<(usize, f64)> = Vec::new();
    for t in req["tracks"].as_array().cloned().unwrap_or_default() {
        let (Some(i), muted) = (t["index"].as_u64().map(|x| x as usize), t["mute"].as_bool().unwrap_or(false)) else { continue };
        let g = t["gain"].as_f64().unwrap_or(1.0).clamp(0.0, 1.5);
        if i < ntracks && !muted && g > 0.0 && !sel.iter().any(|(j, _)| *j == i) {
            sel.push((i, g));
        }
    }
    if sel.is_empty() {
        return (s(&["-an"]), "sin audio (todas las pistas silenciadas)".into());
    }
    let mut graph = String::new();
    for (k, (i, g)) in sel.iter().enumerate() {
        graph.push_str(&format!("[0:a:{i}]volume={g:.3}[v{k}];"));
    }
    let label = if sel.len() == 1 {
        graph.pop(); // sin «;» final
        "[v0]".to_string()
    } else {
        for k in 0..sel.len() {
            graph.push_str(&format!("[v{k}]"));
        }
        graph.push_str(&format!("amix=inputs={}:normalize=0:duration=longest,alimiter=limit=0.95[aout]", sel.len()));
        "[aout]".to_string()
    };
    let mut a = vec!["-filter_complex".to_string(), graph, "-map".to_string(), label];
    a.extend(s(&["-c:a", "aac", "-ac", "2", "-b:a"]));
    a.push(format!("{kbps}k"));
    (a, format!("mezcla personalizada de {} pista(s)", sel.len()))
}

fn thumb_path(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    let src = clip_path(ctx, name)?;
    let dir = ctx.dir.join("thumbs");
    let out = dir.join(format!("{name}.jpg"));
    let src_m = fs::metadata(&src).ok()?.modified().ok()?;
    if let Ok(m) = fs::metadata(&out).and_then(|m| m.modified()) {
        if m >= src_m {
            return Some(out);
        }
    }
    fs::create_dir_all(&dir).ok()?;
    let _turn = light_lock();
    if let Ok(m) = fs::metadata(&out).and_then(|m| m.modified()) {
        if m >= src_m {
            return Some(out);
        }
    }
    let dur = probe(&ctx.paths.ffmpeg, &src).map(|p| p.duration).unwrap_or(0.0);
    let at = if dur > 2.0 { 1.0 } else { dur / 2.0 };
    let tmp = dir.join(format!("{name}.tmp.jpg"));
    let args: Vec<String> = [
        "-hide_banner", "-loglevel", "error", "-y", "-ss", &format!("{at:.2}"), "-i", &src.to_string_lossy(),
        "-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "5", &tmp.to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    run_ff(&ctx.paths.ffmpeg, &args).ok()?;
    fs::rename(&tmp, &out).ok()?;
    out.is_file().then_some(out)
}

fn export(ctx: &Ctx, req: &Value) -> Result<Value, String> {
    let name = req["name"].as_str().ok_or("falta el nombre del clip")?;
    let src = clip_path(ctx, name).ok_or("clip no válido")?;
    let mode = req["mode"].as_str().unwrap_or("fast");
    let pr = probe(&ctx.paths.ffmpeg, &src)?;
    let start = req["start"].as_f64().ok_or("falta el inicio")?.max(0.0);
    let end = req["end"].as_f64().ok_or("falta el fin")?.min(pr.duration);
    if !(end - start >= 0.3) {
        return Err("la selección es demasiado corta (mínimo 0,3 s)".into());
    }
    let dur = end - start;
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();
    let ff = &ctx.paths.ffmpeg;
    let s = |x: f64| format!("{x:.3}");
    let base = || -> Vec<String> {
        let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-ss"].iter().map(|x| x.to_string()).collect();
        a.push(s(start));
        a.push("-i".into());
        a.push(src.to_string_lossy().into_owned());
        a.push("-t".into());
        a.push(s(dur));
        a
    };
    let size_mb = |p: &Path| fs::metadata(p).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);
    match mode {
        "fast" => {
            let out = unique_path(&ctx.paths.out_dir, &stem, "_recorte");
            let mut a = base();
            // -map 0: conserva el vídeo y todas las pistas de audio tal cual (el mezclador no se aplica aquí).
            a.extend(["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero", "-movflags", "+faststart"].map(String::from));
            a.push(out.to_string_lossy().into_owned());
            run_ff(ff, &a)?;
            let real = probe(ff, &out).map(|p| p.duration).unwrap_or(dur);
            let note = if real > dur + 0.5 {
                format!("Recorte rápido sin recodificar: dura {real:.1} s en vez de {dur:.1} s porque el inicio retrocedió al fotograma clave anterior. Para cortar justo donde marcaste usa «Exacto».")
            } else {
                format!("Recorte rápido sin recodificar ({real:.1} s).")
            };
            Ok(json!({"ok": true, "file": out.file_name().unwrap().to_string_lossy(), "size_mb": size_mb(&out), "note": note}))
        }
        "exact" => {
            let out = unique_path(&ctx.paths.out_dir, &stem, "_exacto");
            let (aud, adesc) = audio_out_args(req, pr.audio.len(), 160);
            let build = |fam: encoding::Family| {
                let mut a = base();
                a.extend(["-map", "0:v:0"].map(String::from));
                a.extend(aud.clone());
                a.extend(encoding::exact_args(fam));
                a.extend(["-movflags", "+faststart"].map(String::from));
                a.push(out.to_string_lossy().into_owned());
                a
            };
            // Codificador por hardware y, si no está disponible en este equipo, CPU.
            let fam = encoding::family(&encoding::resolve(&ctx.cfg.encoder));
            if let Err(e) = run_ff(ff, &build(fam)) {
                if fam == encoding::Family::Cpu {
                    return Err(e);
                }
                log(&ctx.paths.log, &format!("exportar: el codificador por hardware falló ({e}); uso CPU"));
                run_ff(ff, &build(encoding::Family::Cpu))?;
            }
            Ok(json!({"ok": true, "file": out.file_name().unwrap().to_string_lossy(), "size_mb": size_mb(&out),
                "note": format!("Recorte exacto: recodificado a H.264 (compatible en todas partes). Audio: {adesc}.")}))
        }
        "discord" => {
            let target_mb = req["target_mb"].as_f64().unwrap_or(ctx.cfg.discord_limit_mb as f64).clamp(1.0, 500.0);
            let limit = target_mb * 1048576.0;
            let out = unique_path(&ctx.paths.out_dir, &stem, "_discord");
            let mut vk = (target_mb * 8192.0 / dur - 96.0).floor();
            if vk < 120.0 {
                return Err(format!("con {target_mb:.1} MB no cabe una selección de {dur:.0} s; acórtala o sube el límite"));
            }
            let mut last_mb = 0.0;
            let mut fam = encoding::family(&encoding::resolve(&ctx.cfg.encoder));
            let (aud, adesc) = audio_out_args(req, pr.audio.len(), 96);
            for _ in 0..4 {
                let h = if vk >= 2500.0 { 1080 } else if vk >= 1200.0 { 720 } else { 480 };
                let h = if pr.height > 0 { h.min(pr.height) } else { h };
                let build = |fam: encoding::Family| {
                    let mut a = base();
                    a.extend(["-map", "0:v:0"].map(String::from));
                    a.extend(aud.clone());
                    a.extend(["-vf".to_string(), format!("scale=-2:{h}")]);
                    a.extend(encoding::discord_args(fam, vk as u32));
                    a.extend(["-movflags".to_string(), "+faststart".into()]);
                    a.push(out.to_string_lossy().into_owned());
                    a
                };
                if let Err(e) = run_ff(ff, &build(fam)) {
                    if fam == encoding::Family::Cpu {
                        return Err(e);
                    }
                    log(&ctx.paths.log, &format!("exportar: el codificador por hardware falló ({e}); uso CPU"));
                    fam = encoding::Family::Cpu;
                    run_ff(ff, &build(fam))?;
                }
                let size = fs::metadata(&out).map(|m| m.len() as f64).unwrap_or(0.0);
                last_mb = size / 1048576.0;
                if size <= limit {
                    return Ok(json!({"ok": true, "file": out.file_name().unwrap().to_string_lossy(), "size_mb": last_mb,
                        "note": format!("Comprimido a {last_mb:.1} MB (límite {target_mb:.1} MB), H.264 + AAC, compatible con Discord. Audio: {adesc}.")}));
                }
                vk = (vk * (limit / size) * 0.92).floor();
                if vk < 120.0 {
                    break;
                }
            }
            let _ = fs::remove_file(&out);
            Err(format!("no logré bajar de {target_mb:.1} MB (llegué a {last_mb:.1} MB); acorta la selección o sube el límite"))
        }
        _ => Err("modo de exportación desconocido".into()),
    }
}

// ---------- datos para la página ----------

fn list_clips(ctx: &Ctx) -> Vec<Value> {
    let mut v: Vec<(u64, String, u64, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&ctx.paths.out_dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !valid_name(&n) {
                continue;
            }
            if let Ok(m) = e.metadata() {
                let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                v.push((t, n, m.len(), e.path()));
            }
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter()
        .map(|(t, n, len, p)| {
            let kind = if n.contains("_discord") { "discord" } else if n.contains("_recorte") || n.contains("_exacto") { "recorte" } else { "clip" };
            let (dur, tracks) = clip_meta(ctx, &n, &p, len, t);
            json!({"name": n, "size_mb": len as f64 / 1048576.0, "mtime": t, "duration": dur, "kind": kind, "tracks": tracks})
        })
        .collect()
}

fn status_json(ctx: &Ctx) -> Value {
    let (state, ram, buf) = {
        let s = STATUS.lock().unwrap();
        (if s.state.is_empty() { "stopped".to_string() } else { s.state.clone() }, s.ram_mb, s.buffer_secs)
    };
    let events: Vec<Value> = EVENTS
        .lock()
        .unwrap()
        .iter()
        .rev()
        .take(8)
        .map(|(t, x)| json!({"t": t, "text": x}))
        .collect();
    let clips = list_clips(ctx);
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    json!({
        "state": state,
        "buffer_secs": buf,
        "buffer_target": ctx.cfg.buffer_seconds,
        "res": format!("{w}×{h}"),
        "fps": ctx.cfg.fps,
        "encoder": if ctx.cfg.encoder == "auto" { format!("auto ({})", encoding::resolve("auto")) } else { ctx.cfg.encoder.clone() },
        "bitrate": ctx.cfg.bitrate_mbps,
        "audio_enabled": ctx.cfg.audio.enabled,
        "audio_kbps": ctx.cfg.audio.bitrate_kbps,
        "ram_mb": ram,
        "hotkey": ctx.cfg.hotkey,
        "theme": ctx.cfg.theme,
        "dir": ctx.paths.out_dir.to_string_lossy(),
        "tracks": crate::audio::TRACKS.lock().map(|t| serde_json::to_value(&*t).unwrap_or(Value::Null)).unwrap_or(Value::Null),
        "last_clip": clips.first().cloned().unwrap_or(Value::Null),
        "clip_count": clips.len(),
        "clips_mb": clips.iter().map(|c| c["size_mb"].as_f64().unwrap_or(0.0)).sum::<f64>(),
        "events": events,
    })
}

const ENCODERS: [&str; 6] = ["auto", "h264_nvenc", "hevc_nvenc", "av1_nvenc", "h264_amf", "libx264"];

/// Valida y normaliza la configuración recibida de la página. `current` aporta lo que la página no puede cambiar.
fn validate(mut c: Config, current: &Config) -> Result<Config, String> {
    c.ffmpeg_path = current.ffmpeg_path.clone(); // la ruta de ffmpeg solo se edita en config.json
    c.buffer_seconds = c.buffer_seconds.clamp(10, 600);
    c.segment_seconds = c.segment_seconds.clamp(1, 10);
    c.fps = c.fps.clamp(10, 240);
    c.monitor = c.monitor.min(7);
    c.bitrate_mbps = c.bitrate_mbps.clamp(1, 200);
    c.maxrate_mbps = c.maxrate_mbps.clamp(c.bitrate_mbps, 300);
    c.audio.bitrate_kbps = c.audio.bitrate_kbps.clamp(32, 512);
    c.discord_limit_mb = c.discord_limit_mb.clamp(1.0, 500.0);
    if !ENCODERS.contains(&c.encoder.as_str()) {
        return Err(format!("codificador no admitido: {}", c.encoder));
    }
    if !(c.nvenc_preset.len() == 2 && c.nvenc_preset.starts_with('p') && matches!(c.nvenc_preset.as_bytes()[1], b'1'..=b'7')) {
        return Err("preset NVENC no válido (p1 a p7)".into());
    }
    if c.theme != "light" {
        c.theme = "dark".into();
    }
    // Pistas de audio: aplicaciones con nombre de ejecutable simple (sin rutas) y sin repetidos.
    {
        let t = &mut c.audio.tracks;
        t.mic_device = t.mic_device.trim().chars().filter(|ch| !ch.is_control()).take(120).collect();
        if !(0.0..=30.0).contains(&t.mic_gain_db) {
            return Err("la ganancia del micrófono va de 0 a 30 dB".into());
        }
        if t.apps.len() > 8 {
            return Err("como máximo 8 aplicaciones con pista propia".into());
        }
        let mut seen = std::collections::HashSet::new();
        for a in t.apps.iter_mut() {
            a.name = a.name.trim().chars().filter(|ch| !ch.is_control()).take(30).collect();
            a.exe = a.exe.trim().to_string();
            let ok = a.exe.len() > 4
                && a.exe.len() <= 60
                && a.exe.to_ascii_lowercase().ends_with(".exe")
                && a.exe.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ' '));
            if !ok {
                return Err(format!("ejecutable no válido: «{}» (solo el nombre, por ejemplo Discord.exe)", a.exe));
            }
            if a.name.is_empty() {
                a.name = a.exe[..a.exe.len() - 4].to_string();
            }
            if !seen.insert(a.exe.to_ascii_lowercase()) {
                return Err(format!("la aplicación {} está repetida", a.exe));
            }
        }
    }
    c.hotkey = c.hotkey.trim().to_string();
    if c.hotkey.parse::<global_hotkey::hotkey::HotKey>().is_err() {
        return Err(format!("atajo no válido: «{}»", c.hotkey));
    }
    if !c.output_dir.is_empty() {
        let p = Path::new(&c.output_dir);
        if !p.is_absolute() {
            return Err("la carpeta de clips debe ser una ruta absoluta, por ejemplo C:\\Clips".into());
        }
        fs::create_dir_all(p).map_err(|e| format!("no pude crear la carpeta de clips: {e}"))?;
    }
    Ok(c)
}

fn write_config(dir: &Path, c: &Config) -> Result<(), String> {
    let json = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    fs::write(dir.join("config.json"), json).map_err(|e| format!("no pude guardar config.json: {e}")) // sin BOM
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Clipper";

/// Argumento con el que Windows inicia Clipper al abrir sesión: arranca sin abrir la ventana.
/// (Al abrirlo a mano, sin este argumento, sí se abre la ventana.)
pub const BACKGROUND_ARG: &str = "--background";

/// Valor actual de la clave Run de Clipper (en minúsculas), si existe.
fn autostart_value() -> Option<String> {
    match silent_cmd(Path::new("reg.exe")).args(["query", RUN_KEY, "/v", RUN_NAME]).stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).to_lowercase()),
        _ => None,
    }
}

/// ¿Está este ejecutable registrado para iniciar con Windows (clave Run del usuario)?
pub fn autostart_enabled() -> bool {
    let exe = std::env::current_exe().unwrap_or_default().to_string_lossy().to_lowercase();
    autostart_value().map(|v| v.contains(&exe)).unwrap_or(false)
}

/// Si el inicio con Windows ya estaba activado con una versión anterior (sin `--background`), lo actualiza para que
/// el arranque automático no abra la ventana. No activa nada que el usuario no tuviera activado.
pub fn migrate_autostart() {
    let exe = std::env::current_exe().unwrap_or_default().to_string_lossy().to_lowercase();
    if let Some(v) = autostart_value() {
        if needs_background_arg(&v, &exe) {
            let _ = set_autostart(true);
        }
    }
}

/// ¿El valor de la clave Run es de este ejecutable pero todavía no lleva `--background`? (ambos en minúsculas)
fn needs_background_arg(run_value: &str, exe_lower: &str) -> bool {
    run_value.contains(exe_lower) && !run_value.contains(BACKGROUND_ARG)
}

/// Activa o quita el inicio con Windows. Solo toca el valor «Clipper» de la clave Run del usuario actual.
fn set_autostart(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut c = silent_cmd(Path::new("reg.exe"));
    if on {
        c.args(["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d"]).arg(format!("\"{}\" {BACKGROUND_ARG}", exe.display())).arg("/f");
    } else {
        c.args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"]);
    }
    let out = c.stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if autostart_enabled() == on {
        return Ok(()); // incluye quitar un valor que ya no existía
    }
    let why = String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("").trim().to_string();
    Err(if why.is_empty() { "no se pudo cambiar la clave de inicio".into() } else { why })
}

/// Envía un archivo a la Papelera de reciclaje (recuperable), sin diálogos ni confirmación del sistema.
fn trash(path: &Path) -> Result<(), String> {
    let script = "Add-Type -AssemblyName Microsoft.VisualBasic; \
        [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($env:CLIPPER_FILE,'OnlyErrorDialogs','SendToRecycleBin')";
    let out = silent_cmd(Path::new("powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("CLIPPER_FILE", path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() && !path.exists() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("").trim().to_string();
        Err(if why.is_empty() { "no se pudo mover a la papelera".into() } else { why })
    }
}

fn pick_folder(initial: &str) -> Option<String> {
    let script = "Add-Type -AssemblyName System.Windows.Forms; $f=New-Object System.Windows.Forms.Form; $f.TopMost=$true; \
        $d=New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description='Carpeta de clips'; \
        if($env:CLIPPER_INIT -and (Test-Path -LiteralPath $env:CLIPPER_INIT)){ $d.SelectedPath=$env:CLIPPER_INIT }; \
        if($d.ShowDialog($f) -eq 'OK'){ [Console]::Out.Write($d.SelectedPath) }";
    let out = silent_cmd(Path::new("powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
        .env("CLIPPER_INIT", initial)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn handle(mut stream: TcpStream, ui: Arc<Ui>) {
    let Some(req) = read_request(&mut stream) else { return };
    // Defensa contra DNS rebinding: solo se acepta el host local con nuestro puerto.
    let host_ok = req
        .headers
        .get("host")
        .map(|h| h == &format!("127.0.0.1:{}", ui.port) || h == &format!("localhost:{}", ui.port))
        .unwrap_or(false);
    if !host_ok {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"host", &[]);
    }
    if req.query.get("t").map(|t| t.as_str()) != Some(ui.token.as_str()) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"token", &[]);
    }
    let (cfg, _) = load_config(&ui.dir);
    let paths = resolve_paths(&cfg, &ui.dir);
    let ctx = Ctx { cfg, paths, dir: ui.dir.clone() };
    let body = || serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            let html = PAGE.replace("__THEME__", if ctx.cfg.theme == "light" { "light" } else { "dark" });
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", html.as_bytes(), &[]);
        }
        ("GET", "/api/status") => respond_json(&mut stream, &status_json(&ctx)),
        ("GET", "/api/clips") => respond_json(&mut stream, &Value::Array(list_clips(&ctx))),
        ("GET", "/api/config") => respond_json(
            &mut stream,
            &json!({"config": ctx.cfg, "defaults": Config::default(), "encoders": ENCODERS, "autostart": autostart_enabled()}),
        ),
        ("POST", "/api/autostart") => {
            let on = body()["enabled"].as_bool().unwrap_or(false);
            match set_autostart(on) {
                Ok(()) => {
                    log(&ctx.paths.log, if on { "inicio con Windows: activado" } else { "inicio con Windows: desactivado" });
                    respond_json(&mut stream, &json!({"ok": true, "enabled": autostart_enabled()}))
                }
                Err(e) => respond_json(&mut stream, &json!({"ok": false, "error": e, "enabled": autostart_enabled()})),
            }
        }
        ("GET", p) if p.starts_with("/video/") => match clip_path(&ctx, &p[7..]) {
            Some(path) => serve_file(&mut stream, &path, req.headers.get("range").map(|s| s.as_str())),
            None => respond(&mut stream, "404 Not Found", "text/plain", b"no existe", &[]),
        },
        ("GET", "/api/peaks") => {
            let name = req.query.get("name").cloned().unwrap_or_default();
            let track = req.query.get("track").and_then(|t| t.parse::<usize>().ok()).unwrap_or(0);
            let n = req.query.get("n").and_then(|t| t.parse::<usize>().ok()).unwrap_or(200).clamp(20, 600);
            match peaks(&ctx, &name, track, n) {
                Ok(v) => respond_json(&mut stream, &json!({"ok": true, "peaks": v})),
                Err(e) => respond_json(&mut stream, &json!({"ok": false, "error": e})),
            }
        }
        ("GET", "/api/audio-apps") => respond_json(&mut stream, &json!({"apps": crate::audio::visible_window_exes()})),
        ("GET", "/api/audio-devices") => respond_json(&mut stream, &json!({"devices": crate::audio::mic_devices()})),
        ("GET", p) if p.starts_with("/audio/") => {
            let rest = &p[7..];
            let path = rest.rsplit_once('/').and_then(|(name, i)| i.parse::<usize>().ok().and_then(|i| track_audio_path(&ctx, name, i)));
            match path {
                Some(path) => serve_file(&mut stream, &path, req.headers.get("range").map(|s| s.as_str())),
                None => respond(&mut stream, "404 Not Found", "text/plain", b"sin pista", &[]),
            }
        }
        ("GET", p) if p.starts_with("/thumb/") => match thumb_path(&ctx, &p[7..]) {
            Some(path) => serve_file(&mut stream, &path, None),
            None => respond(&mut stream, "404 Not Found", "text/plain", b"sin miniatura", &[]),
        },
        ("POST", "/api/save") => {
            log(&ctx.paths.log, "guardar clip pedido desde la ventana");
            let (cfg, paths) = (ctx.cfg.clone(), Arc::new(resolve_paths(&ctx.cfg, &ctx.dir)));
            match save_clip(cfg, paths) {
                Ok(p) => {
                    let mb = fs::metadata(&p).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);
                    respond_json(&mut stream, &json!({"ok": true, "file": p.file_name().map(|n| n.to_string_lossy().into_owned()), "size_mb": mb}))
                }
                Err(e) => respond_json(&mut stream, &json!({"ok": false, "error": e})),
            }
        }
        ("POST", "/api/export") => {
            let r = body();
            log(&ctx.paths.log, &format!("interfaz: exportar {} ({})", r["name"].as_str().unwrap_or("?"), r["mode"].as_str().unwrap_or("?")));
            match export(&ctx, &r) {
                Ok(v) => {
                    log(&ctx.paths.log, &format!("clip exportado: {}", v["file"]));
                    respond_json(&mut stream, &v)
                }
                Err(e) => {
                    log(&ctx.paths.log, &format!("error al exportar: {e}"));
                    respond_json(&mut stream, &json!({"ok": false, "error": e}))
                }
            }
        }
        ("POST", "/api/config") => {
            let r = body();
            let result = serde_json::from_value::<Config>(r)
                .map_err(|e| format!("configuración no válida: {e}"))
                .and_then(|c| validate(c, &ctx.cfg))
                .and_then(|c| write_config(&ctx.dir, &c).map(|_| c));
            match result {
                Ok(c) => {
                    RELOAD.store(true, Ordering::Relaxed);
                    respond_json(&mut stream, &json!({"ok": true, "config": c}))
                }
                Err(e) => respond_json(&mut stream, &json!({"ok": false, "error": e})),
            }
        }
        ("POST", "/api/theme") => {
            let r = body();
            let mut c = ctx.cfg.clone();
            c.theme = if r["theme"].as_str() == Some("light") { "light".into() } else { "dark".into() };
            match write_config(&ctx.dir, &c) {
                Ok(()) => respond_json(&mut stream, &json!({"ok": true})), // el tema no reinicia la grabación
                Err(e) => respond_json(&mut stream, &json!({"ok": false, "error": e})),
            }
        }
        ("POST", "/api/pick-folder") => {
            let init = ctx.paths.out_dir.to_string_lossy().into_owned();
            match pick_folder(&init) {
                Some(p) => respond_json(&mut stream, &json!({"ok": true, "path": p})),
                None => respond_json(&mut stream, &json!({"ok": false, "cancelled": true})),
            }
        }
        ("POST", "/api/copy") => {
            let r = body();
            match r["file"].as_str().and_then(|n| clip_path(&ctx, n)) {
                Some(p) => {
                    // Set-Clipboard -LiteralPath deja el archivo en el portapapeles como al copiarlo en el Explorador.
                    // La ruta viaja por una variable de entorno: con -Command, PowerShell no rellena $args.
                    let out = silent_cmd(Path::new("powershell.exe"))
                        .args(["-NoProfile", "-NonInteractive", "-Command", "Set-Clipboard -LiteralPath $env:CLIPPER_FILE"])
                        .env("CLIPPER_FILE", &p)
                        .stdin(Stdio::null())
                        .output();
                    let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
                    if ok {
                        log(&ctx.paths.log, &format!("archivo copiado al portapapeles: {}", p.file_name().unwrap_or_default().to_string_lossy()));
                    } else {
                        let why = out.map(|o| String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").to_string()).unwrap_or_default();
                        log(&ctx.paths.log, &format!("no pude copiar al portapapeles: {why}"));
                    }
                    respond_json(&mut stream, &json!({"ok": ok}))
                }
                None => respond_json(&mut stream, &json!({"ok": false, "error": "archivo no válido"})),
            }
        }
        ("POST", "/api/reveal") => {
            let r = body();
            match r["file"].as_str().and_then(|n| clip_path(&ctx, n)) {
                Some(p) => {
                    let _ = Command::new("explorer.exe").arg(format!("/select,{}", p.display())).spawn();
                    respond_json(&mut stream, &json!({"ok": true}))
                }
                None => {
                    // Sin archivo: abre la carpeta de clips.
                    let _ = Command::new("explorer.exe").arg(&ctx.paths.out_dir).spawn();
                    respond_json(&mut stream, &json!({"ok": true}))
                }
            }
        }
        ("POST", "/api/delete") => {
            let r = body();
            let names: Vec<String> = r["files"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).take(200).collect())
                .unwrap_or_default();
            let (mut deleted, mut failed) = (Vec::new(), Vec::new());
            for n in &names {
                match clip_path(&ctx, n) {
                    Some(p) => match trash(&p) {
                        Ok(()) => {
                            // miniatura y pistas de audio extraídas de ese clip
                            if let Ok(rd) = fs::read_dir(ctx.dir.join("thumbs")) {
                                for f in rd.flatten() {
                                    if f.file_name().to_string_lossy().starts_with(&format!("{n}.")) {
                                        let _ = fs::remove_file(f.path());
                                    }
                                }
                            }
                            meta_cache().lock().unwrap().remove(n);
                            log(&ctx.paths.log, &format!("clip eliminado (a la papelera): {n}"));
                            deleted.push(n.clone());
                        }
                        Err(e) => {
                            log(&ctx.paths.log, &format!("no pude eliminar {n}: {e}"));
                            failed.push(json!({"name": n, "error": e}));
                        }
                    },
                    None => failed.push(json!({"name": n, "error": "el clip no existe"})),
                }
            }
            respond_json(&mut stream, &json!({"ok": failed.is_empty() && !deleted.is_empty(), "deleted": deleted, "failed": failed}));
        }
        ("POST", "/api/quit") => {
            QUIT.store(true, Ordering::Relaxed);
            respond(&mut stream, "200 OK", "text/plain", b"ok", &[]);
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"no encontrado", &[]),
    }
}

/// Arranca el servidor en un puerto local libre y devuelve su dirección y token.
pub fn start(dir: PathBuf, log_path: &Path) -> Option<Arc<Ui>> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            log(log_path, &format!("interfaz: no pude abrir el puerto local: {e}"));
            return None;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let ui = Arc::new(Ui { token: make_token(), port, dir });
    log(log_path, &format!("interfaz: servidor local en el puerto {port}"));
    let ui2 = ui.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let u = ui2.clone();
            thread::spawn(move || handle(conn, u));
        }
    });
    Some(ui)
}

/// Navegador con modo `--app` (ventana sin barras): Chrome primero, luego Edge.
fn browser_path() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut c: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(format!(r"{local}\Google\Chrome\Application\chrome.exe")),
        PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
    ];
    c.retain(|p| p.exists());
    c.into_iter().next()
}

/// Abre la ventana de la interfaz. Perfil propio: no toca el navegador del usuario.
pub fn open_window(ui: &Ui, view: Option<&str>) {
    let mut url = ui.url();
    if let Some(v) = view {
        url.push_str(&format!("#{v}"));
    }
    open_url(&ui.dir, &url);
}

/// Abre una dirección de la interfaz en una ventana de aplicación (Chrome/Edge) o, si no hay, en el navegador por defecto.
pub fn open_url(dir: &Path, url: &str) {
    log(&dir.join("clipper.log"), "interfaz: abriendo la ventana");
    if std::env::var_os("CLIPPER_NO_BROWSER").is_some() {
        return; // ayuda para las pruebas automáticas: no abre ninguna ventana
    }
    if let Some(exe) = browser_path() {
        let profile = dir.join("ui_profile");
        let _ = Command::new(exe)
            .arg(format!("--app={url}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--window-size=1100,780", "--no-first-run", "--no-default-browser-check", "--disable-extensions"])
            .spawn();
    } else {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config::default()
    }

    #[test]
    fn migracion_del_inicio_con_windows() {
        let exe = r"d:\clipper\clipper.exe";
        // valor de una versión anterior (sin argumento): hay que actualizarlo
        assert!(needs_background_arg("    clipper    reg_sz    \"d:\\clipper\\clipper.exe\"\r\n", exe));
        // ya migrado: no se toca
        assert!(!needs_background_arg("    clipper    reg_sz    \"d:\\clipper\\clipper.exe\" --background\r\n", exe));
        // el valor es de otra copia de Clipper: no es asunto de este ejecutable
        assert!(!needs_background_arg("    clipper    reg_sz    \"e:\\otra\\clipper.exe\"\r\n", exe));
        assert_eq!(BACKGROUND_ARG, "--background");
    }

    #[test]
    fn validate_ajusta_rangos_y_conserva_ffmpeg() {
        let mut c = base();
        c.buffer_seconds = 5000;
        c.bitrate_mbps = 50;
        c.maxrate_mbps = 10; // menor que el bitrate: se sube
        c.ffmpeg_path = "C:\\malo\\evil.exe".into();
        let mut cur = base();
        cur.ffmpeg_path = "ffmpeg".into();
        let v = validate(c, &cur).unwrap();
        assert_eq!(v.buffer_seconds, 600);
        assert_eq!(v.maxrate_mbps, 50);
        assert_eq!(v.ffmpeg_path, "ffmpeg", "la página no puede cambiar la ruta de ffmpeg");
    }

    #[test]
    fn validate_rechaza_datos_malos() {
        let cur = base();
        let mut c = base();
        c.encoder = "rm -rf".into();
        assert!(validate(c, &cur).is_err());
        let mut c = base();
        c.hotkey = "no es un atajo".into();
        assert!(validate(c, &cur).is_err());
        let mut c = base();
        c.nvenc_preset = "p9".into();
        assert!(validate(c, &cur).is_err());
        let mut c = base();
        c.output_dir = "clips_relativos".into();
        assert!(validate(c, &cur).is_err());
    }

    #[test]
    fn validate_pistas_de_audio() {
        use crate::AppTrack;
        let cur = base();
        let mut c = base();
        c.audio.tracks.apps.push(AppTrack { name: "  Spotify  ".into(), exe: "Spotify.exe".into(), enabled: true });
        let v = validate(c, &cur).unwrap();
        assert_eq!(v.audio.tracks.apps.last().unwrap().name, "Spotify");
        let mut c = base();
        c.audio.tracks.mic_gain_db = 12.0;
        assert_eq!(validate(c, &cur).unwrap().audio.tracks.mic_gain_db, 12.0);
        for bad in [-1.0f32, 45.0, f32::NAN] {
            let mut c = base();
            c.audio.tracks.mic_gain_db = bad;
            assert!(validate(c, &cur).is_err(), "ganancia fuera de rango: {bad}");
        }
        let mut c = base();
        c.audio.tracks.apps[0].exe = "C:\\malo\\x.exe".into();
        assert!(validate(c, &cur).is_err(), "no se admiten rutas");
        let mut c = base();
        c.audio.tracks.apps[0].exe = "notepad.txt".into();
        assert!(validate(c, &cur).is_err());
        let mut c = base();
        c.audio.tracks.apps.push(AppTrack { name: "Otra".into(), exe: "DISCORD.EXE".into(), enabled: true });
        assert!(validate(c, &cur).is_err(), "repetida sin distinguir mayúsculas");
        let mut c = base();
        for i in 0..9 {
            c.audio.tracks.apps.push(AppTrack { name: format!("a{i}"), exe: format!("a{i}.exe"), enabled: true });
        }
        assert!(validate(c, &cur).is_err(), "demasiadas aplicaciones");
    }

    #[test]
    fn nombres_de_pistas_de_audio() {
        let info = "  Stream #0:0[0x1](und): Video: h264 (High), yuv420p, 1920x1080\n    Metadata:\n      handler_name    : VideoHandler\n  Stream #0:1[0x2](und): Audio: aac (LC), 48000 Hz\n    Metadata:\n      handler_name    : Sistema\n  Stream #0:2[0x3](und): Audio: aac (LC), 48000 Hz\n    Metadata:\n      handler_name    : SoundHandler\n  Stream #0:3[0x4](und): Audio: aac (LC), 48000 Hz\n    Metadata:\n      title           : Discord\n      handler_name    : SoundHandler\n";
        assert_eq!(audio_track_names(info), ["Sistema", "Pista 2", "Discord"]);
    }

    #[test]
    fn mezcla_personalizada_de_pistas() {
        let req = json!({"audio_mode": "custom", "tracks": [
            {"index": 1, "gain": 1.0, "mute": false}, {"index": 2, "gain": 0.6, "mute": false}, {"index": 3, "gain": 1.0, "mute": true}]});
        let (a, desc) = audio_out_args(&req, 4, 160);
        let j = a.join(" ");
        assert!(j.contains("[0:a:1]volume=1.000[v0];[0:a:2]volume=0.600[v1];[v0][v1]amix=inputs=2:normalize=0"), "{j}");
        assert!(!j.contains("0:a:3"), "la pista silenciada no entra");
        assert!(desc.contains("2 pista"));
        // una sola pista elegida: sin amix
        let one = json!({"audio_mode": "custom", "tracks": [{"index": 2, "gain": 1.5, "mute": false}]});
        let j1 = audio_out_args(&one, 4, 96).0.join(" ");
        assert!(j1.contains("[0:a:2]volume=1.500[v0] -map [v0]"), "{j1}");
        // todo silenciado
        let none = json!({"audio_mode": "custom", "tracks": [{"index": 1, "mute": true}]});
        assert_eq!(audio_out_args(&none, 4, 160).0, ["-an"]);
        // mezcla completa: la pista Sistema tal cual
        let full = json!({"audio_mode": "full"});
        assert!(audio_out_args(&full, 4, 160).0.join(" ").contains("-map 0:a:0"));
        // mezcla completa con micrófono (pista 4): Sistema + micrófono; si se silencia, vuelve a la pista Sistema
        let withmic = json!({"audio_mode": "full", "mic_index": 4, "tracks": [{"index": 4, "gain": 1.0, "mute": false}]});
        let (a, d) = audio_out_args(&withmic, 5, 160);
        assert!(a.join(" ").contains("[0:a:0][0:a:4]amix=inputs=2:normalize=0"), "{a:?}");
        assert!(d.contains("micrófono"));
        let quiet = json!({"audio_mode": "full", "mic_index": 4, "tracks": [{"index": 4, "gain": 0.5, "mute": false}]});
        assert!(audio_out_args(&quiet, 5, 160).0.join(" ").contains("[0:a:4]volume=0.500[mv];[0:a:0][mv]amix"));
        let muted = json!({"audio_mode": "full", "mic_index": 4, "tracks": [{"index": 4, "gain": 1.0, "mute": true}]});
        assert!(audio_out_args(&muted, 5, 160).0.join(" ").contains("-map 0:a:0"));
        assert!(!audio_out_args(&json!({"audio_mode": "full", "mic_index": 9}), 5, 160).0.join(" ").contains("amix"), "índice inválido");
        // sin audio en el clip
        assert_eq!(audio_out_args(&full, 0, 160).0, ["-an"]);
    }

    #[test]
    fn nombres_de_clip() {
        assert!(valid_name("clip_20261016_021118.mp4"));
        assert!(!valid_name("..\\config.json"));
        assert!(!valid_name("../x.mp4"));
        assert!(!valid_name("config.json"));
        assert!(!valid_name(".oculto.mp4"));
    }

    #[test]
    fn eventos_solo_los_utiles() {
        push_event("ffmpeg -hide_banner -loglevel warning");
        push_event("audio: 44100 Hz");
        push_event("clip guardado: C:\\Clips\\clip_a.mp4");
        let q = EVENTS.lock().unwrap();
        assert!(q.iter().any(|(_, t)| t == "Clip guardado · clip_a.mp4"));
        assert!(!q.iter().any(|(_, t)| t.starts_with("Ffmpeg") || t.starts_with("Audio")));
    }
}
