// Audio por pistas: «Sistema» (mezcla completa), «Juego», una pista por aplicación y «Micrófono».
// Cada pista se captura con WASAPI (por proceso con ActivateAudioInterface de «loopback de aplicación»),
// se convierte a 48 kHz f32 estéreo y un mezclador con reloj propio entrega bloques de 20 ms de todas las
// pistas, intercalados, a ffmpeg por stdin (2 canales por pista). ffmpeg los separa en pistas del archivo.
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use wasapi::{initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::{log, Config};

pub const RATE: u32 = 48_000;
/// 20 ms por bloque.
const FRAMES: usize = 960;
/// Máximo de audio acumulado por pista antes de recortar lo más viejo (evita que la deriva de reloj crezca).
const MAX_QUEUE_BLOCKS: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    System,
    Game,
    App(String),
    Mic(String),
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub title: String,
    pub kind: Kind,
}

/// Estado de una pista para mostrarlo en la interfaz.
#[derive(Clone, Serialize, Default)]
pub struct TrackInfo {
    pub title: String,
    /// "capturing" | "waiting" | "error"
    pub state: String,
    pub detail: String,
}

pub static TRACKS: Mutex<Vec<TrackInfo>> = Mutex::new(Vec::new());

fn set_info(i: usize, state: &str, detail: &str) {
    if let Ok(mut t) = TRACKS.lock() {
        if let Some(x) = t.get_mut(i) {
            x.state = state.to_string();
            x.detail = detail.to_string();
        }
    }
}

/// Pistas que habrá según la configuración: Sistema, Juego, aplicaciones activas y Micrófono.
pub fn specs_from_config(cfg: &Config) -> Vec<Spec> {
    if !cfg.audio.enabled {
        return Vec::new();
    }
    let t = &cfg.audio.tracks;
    let mut v = vec![Spec { title: "Sistema".into(), kind: Kind::System }];
    if t.game {
        v.push(Spec { title: "Juego".into(), kind: Kind::Game });
    }
    for a in t.apps.iter().filter(|a| a.enabled && !a.exe.is_empty()) {
        v.push(Spec { title: if a.name.is_empty() { a.exe.clone() } else { a.name.clone() }, kind: Kind::App(a.exe.clone()) });
    }
    if t.mic {
        v.push(Spec { title: "Micrófono".into(), kind: Kind::Mic(t.mic_device.clone()) });
    }
    v
}

/// Argumentos de ffmpeg para el audio que llega por stdin (`pipe:0`, entrada número 1) y su división en pistas.
/// Devuelve (opciones de entrada, filtro opcional, mapeos y códecs por pista).
pub fn ffmpeg_audio_args(specs: &[Spec], kbps: u32) -> (Vec<String>, Vec<String>) {
    let k = specs.len();
    let mut input: Vec<String> = ["-f", "f32le", "-ar"].iter().map(|s| s.to_string()).collect();
    input.push(RATE.to_string());
    input.push("-ac".into());
    input.push((k * 2).to_string());
    input.extend(["-i", "pipe:0"].map(String::from));
    let mut out: Vec<String> = Vec::new();
    if k == 1 {
        out.extend(["-map", "1:a"].map(String::from));
    } else {
        let mut graph = format!("[1:a]asplit={k}");
        for i in 0..k {
            graph.push_str(&format!("[s{i}]"));
        }
        for i in 0..k {
            graph.push_str(&format!(";[s{i}]pan=stereo|c0=c{}|c1=c{}[a{i}]", i * 2, i * 2 + 1));
        }
        out.push("-filter_complex".into());
        out.push(graph);
        for i in 0..k {
            out.push("-map".into());
            out.push(format!("[a{i}]"));
        }
    }
    out.extend(["-c:a", "aac", "-b:a"].map(String::from));
    out.push(format!("{kbps}k"));
    for (i, s) in specs.iter().enumerate() {
        out.push(format!("-metadata:s:a:{i}"));
        out.push(format!("title={}", s.title));
        out.push(format!("-metadata:s:a:{i}"));
        out.push(format!("handler_name={}", s.title));
    }
    (input, out)
}

// ---------- procesos ----------

#[derive(Clone, Debug)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub exe: String,
}

pub fn snapshot() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    unsafe {
        let Ok(h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(h, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push(ProcEntry { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, exe: String::from_utf16_lossy(&e.szExeFile[..len]) });
                if Process32NextW(h, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(h);
    }
    out
}

/// Proceso raíz de una aplicación: el de ese ejecutable cuyo padre no es otro proceso del mismo ejecutable
/// (la captura por proceso incluye el árbol completo, como las pestañas y el servicio de audio de Chrome).
pub fn root_pid(entries: &[ProcEntry], exe: &str) -> Option<u32> {
    let same: Vec<&ProcEntry> = entries.iter().filter(|e| e.exe.eq_ignore_ascii_case(exe)).collect();
    same.iter().filter(|e| !same.iter().any(|o| o.pid == e.ppid)).map(|e| e.pid).min()
}

/// Ejecutable de la ventana en primer plano (para diagnosticar atajos que otra aplicación pudiera tragarse).
pub fn foreground_exe() -> String {
    let pid = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return "ninguna".into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    };
    snapshot().into_iter().find(|e| e.pid == pid).map(|e| e.exe).unwrap_or_else(|| format!("pid {pid}"))
}

/// PID de la aplicación en primer plano si su ventana cubre todo el monitor (juego a pantalla completa o sin bordes).
pub fn foreground_fullscreen() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        (r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom).then_some(pid)
    }
}

/// Ejecutables que nunca cuentan como «juego» (el escritorio, el menú, Clipper y las apps con pista propia).
fn excluded(exe: &str, apps: &[String]) -> bool {
    const SYS: [&str; 9] = [
        "explorer.exe", "clipper.exe", "applicationframehost.exe", "searchhost.exe", "startmenuexperiencehost.exe",
        "shellexperiencehost.exe", "lockapp.exe", "textinputhost.exe", "dwm.exe",
    ];
    SYS.iter().any(|s| exe.eq_ignore_ascii_case(s)) || apps.iter().any(|a| exe.eq_ignore_ascii_case(a))
}

unsafe extern "system" fn enum_visible(hwnd: HWND, lp: LPARAM) -> BOOL {
    let pids = &mut *(lp.0 as *mut Vec<u32>);
    if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            pids.push(pid);
        }
    }
    BOOL(1)
}

/// Ejecutables con alguna ventana visible: las aplicaciones que el usuario puede querer como pista propia.
pub fn visible_window_exes() -> Vec<String> {
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_visible), LPARAM(&mut pids as *mut Vec<u32> as isize));
    }
    let entries = snapshot();
    let mut out: Vec<String> = pids
        .iter()
        .filter_map(|p| entries.iter().find(|e| e.pid == *p).map(|e| e.exe.clone()))
        .filter(|e| !excluded(e, &[]))
        .collect();
    out.sort_by_key(|s| s.to_lowercase());
    out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    out
}

/// Nombres de los micrófonos (dispositivos de captura) disponibles.
pub fn mic_devices() -> Vec<String> {
    let _ = initialize_mta();
    let r = (|| -> Result<Vec<String>, wasapi::WasapiError> {
        let coll = DeviceEnumerator::new()?.get_device_collection(&Direction::Capture)?;
        let mut v = Vec::new();
        for i in 0..coll.get_nbr_devices()? {
            if let Ok(name) = coll.get_device_at_index(i).and_then(|d| d.get_friendlyname()) {
                v.push(name);
            }
        }
        Ok(v)
    })();
    r.unwrap_or_default()
}

// ---------- captura ----------

#[derive(Clone)]
enum Source {
    System,
    Mic(String),
    Process(u32),
}

type Queue = Arc<Mutex<VecDeque<f32>>>;

/// Pasa las muestras de la captura a la cola; `gain` (lineal) amplifica fuentes muy bajas, como un micrófono USB.
fn push_samples(q: &Queue, bytes: &mut VecDeque<u8>, gain: f32) {
    let mut g = q.lock().unwrap();
    while bytes.len() >= 4 {
        let b = [bytes.pop_front().unwrap(), bytes.pop_front().unwrap(), bytes.pop_front().unwrap(), bytes.pop_front().unwrap()];
        let s = f32::from_le_bytes(b);
        g.push_back(if gain == 1.0 { s } else { (s * gain).clamp(-1.0, 1.0) });
    }
    // Tope de seguridad: si el mezclador no consume, no crece sin límite.
    let max = FRAMES * 2 * MAX_QUEUE_BLOCKS * 2;
    while g.len() > max {
        g.pop_front();
    }
}

/// Captura una fuente a 48 kHz f32 estéreo hasta que se pida parar o falle.
fn capture(source: &Source, q: &Queue, stop: &AtomicBool, gain: f32) -> Result<(), String> {
    let _ = initialize_mta();
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, 2, None);
    let e = |what: &str, err: &dyn std::fmt::Display| format!("{what}: {err}");
    let mut client: AudioClient = match source {
        Source::Process(pid) => AudioClient::new_application_loopback_client(*pid, true).map_err(|x| e("captura de proceso", &x))?,
        Source::System => DeviceEnumerator::new()
            .and_then(|en| en.get_default_device(&Direction::Render))
            .and_then(|d| d.get_iaudioclient())
            .map_err(|x| e("dispositivo de salida", &x))?,
        Source::Mic(name) => {
            let en = DeviceEnumerator::new().map_err(|x| e("enumerador", &x))?;
            let dev = if name.is_empty() {
                en.get_default_device(&Direction::Capture)
            } else {
                en.get_device_collection(&Direction::Capture).and_then(|c| c.get_device_with_name(name))
            }
            .map_err(|x| e("micrófono", &x))?;
            dev.get_iaudioclient().map_err(|x| e("micrófono", &x))?
        }
    };
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client.initialize_client(&fmt, &Direction::Capture, &mode).map_err(|x| e("inicializar", &x))?;
    let ev = client.set_get_eventhandle().map_err(|x| e("evento", &x))?;
    let cap = client.get_audiocaptureclient().map_err(|x| e("cliente de captura", &x))?;
    client.start_stream().map_err(|x| e("iniciar", &x))?;
    let mut raw: VecDeque<u8> = VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        let _ = ev.wait_for_event(100); // sin paquetes cuando nada suena: no es un error
        loop {
            let n = cap.get_next_packet_size().map_err(|x| e("leer", &x))?.unwrap_or(0);
            if n == 0 {
                break;
            }
            cap.read_from_device_to_deque(&mut raw).map_err(|x| e("leer", &x))?;
            push_samples(q, &mut raw, gain);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

struct Running {
    pid: u32,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<(), String>>,
}

/// Gestor de una pista de aplicación o de juego: busca el proceso y (re)inicia su captura cuando cambia.
fn manage_app(idx: usize, kind: Kind, q: Queue, stop: Arc<AtomicBool>, apps: Vec<String>, log_path: Arc<PathBuf>) {
    let mut cur: Option<Running> = None;
    let mut game_pid: Option<u32> = None;
    let mut last_err_at = Instant::now() - Duration::from_secs(60);
    while !stop.load(Ordering::Relaxed) {
        let entries = snapshot();
        let target: Option<(u32, String)> = match &kind {
            Kind::App(exe) => root_pid(&entries, exe).map(|p| (p, exe.clone())),
            Kind::Game => {
                if let Some(fp) = foreground_fullscreen() {
                    if let Some(e) = entries.iter().find(|e| e.pid == fp) {
                        if !excluded(&e.exe, &apps) {
                            game_pid = Some(fp);
                        }
                    }
                }
                // «Pegajoso»: se mantiene el último juego mientras siga vivo.
                game_pid.and_then(|p| entries.iter().find(|e| e.pid == p).map(|e| (p, e.exe.clone())))
            }
            _ => None,
        };
        // ¿terminó la captura actual (error o proceso cerrado)?
        if let Some(r) = &cur {
            if r.handle.is_finished() {
                let r = cur.take().unwrap();
                if let Ok(Err(msg)) = r.handle.join() {
                    set_info(idx, "error", &msg);
                    if last_err_at.elapsed() > Duration::from_secs(30) {
                        log(&log_path, &format!("audio ({}): {msg}", idx));
                        last_err_at = Instant::now();
                    }
                }
            }
        }
        match (&cur, &target) {
            (Some(r), Some((pid, _))) if r.pid == *pid => {}
            _ => {
                if let Some(r) = cur.take() {
                    r.stop.store(true, Ordering::Relaxed);
                    let _ = r.handle.join();
                }
                match target {
                    Some((pid, exe)) => {
                        let s = Arc::new(AtomicBool::new(false));
                        let (q2, s2) = (q.clone(), s.clone());
                        let h = thread::spawn(move || capture(&Source::Process(pid), &q2, &s2, 1.0));
                        cur = Some(Running { pid, stop: s, handle: h });
                        set_info(idx, "capturing", &exe);
                    }
                    None => set_info(idx, "waiting", if matches!(kind, Kind::Game) { "sin juego a pantalla completa" } else { "la aplicación no está abierta" }),
                }
            }
        }
        // Una aplicación que acaba de empezar a sonar se engancha en ≤0,5 s; el juego se vigila cada segundo.
        let ticks = if matches!(kind, Kind::Game) { 10 } else { 5 };
        for _ in 0..ticks {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some(r) = cur.take() {
        r.stop.store(true, Ordering::Relaxed);
        let _ = r.handle.join();
    }
}

/// Mezclador: captura todas las pistas y entrega a ffmpeg bloques intercalados cada 20 ms.
pub struct Mixer {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    /// El dispositivo del sistema falló: hay que reiniciar la grabación.
    pub system_failed: Arc<AtomicBool>,
}

impl Mixer {
    pub fn start(specs: &[Spec], mut stdin: ChildStdin, cfg: &Config, log_path: Arc<PathBuf>) -> Mixer {
        let stop = Arc::new(AtomicBool::new(false));
        let system_failed = Arc::new(AtomicBool::new(false));
        let queues: Vec<Queue> = specs.iter().map(|_| Arc::new(Mutex::new(VecDeque::new()))).collect();
        if let Ok(mut t) = TRACKS.lock() {
            *t = specs.iter().map(|s| TrackInfo { title: s.title.clone(), state: "waiting".into(), detail: String::new() }).collect();
        }
        let app_exes: Vec<String> = cfg.audio.tracks.apps.iter().map(|a| a.exe.clone()).collect();
        let mut handles = Vec::new();
        for (i, s) in specs.iter().enumerate() {
            let (q, st) = (queues[i].clone(), stop.clone());
            match &s.kind {
                Kind::System | Kind::Mic(_) => {
                    let src = if let Kind::Mic(n) = &s.kind { Source::Mic(n.clone()) } else { Source::System };
                    let (is_system, failed, lp) = (matches!(s.kind, Kind::System), system_failed.clone(), log_path.clone());
                    let gain = if is_system { 1.0 } else { 10f32.powf(cfg.audio.tracks.mic_gain_db / 20.0) };
                    handles.push(thread::spawn(move || {
                        set_info(i, "capturing", "");
                        loop {
                            match capture(&src, &q, &st, gain) {
                                Ok(()) => break, // parada pedida
                                Err(msg) => {
                                    set_info(i, "error", &msg);
                                    log(&lp, &format!("audio ({i}): {msg}"));
                                    if is_system {
                                        failed.store(true, Ordering::Relaxed); // la vigilancia reinicia la grabación
                                        break;
                                    }
                                    // micrófono: reintento cada 5 s sin afectar al resto
                                    for _ in 0..50 {
                                        if st.load(Ordering::Relaxed) {
                                            return;
                                        }
                                        thread::sleep(Duration::from_millis(100));
                                    }
                                }
                            }
                        }
                    }));
                }
                Kind::App(_) | Kind::Game => {
                    let (kind, apps, lp) = (s.kind.clone(), app_exes.clone(), log_path.clone());
                    handles.push(thread::spawn(move || manage_app(i, kind, q, st, apps, lp)));
                }
            }
        }
        // Bomba: un bloque de 20 ms por pista con reloj propio; rellena con silencio si una pista no entregó.
        let (qs, st) = (queues, stop.clone());
        handles.push(thread::spawn(move || {
            let k = qs.len();
            let mut out = vec![0f32; FRAMES * 2 * k];
            let mut bytes: Vec<u8> = Vec::with_capacity(out.len() * 4);
            let mut next = Instant::now();
            while !st.load(Ordering::Relaxed) {
                next += Duration::from_millis(20);
                for (ti, q) in qs.iter().enumerate() {
                    let mut g = q.lock().unwrap();
                    let want = FRAMES * 2;
                    if g.len() > want * MAX_QUEUE_BLOCKS {
                        // deriva de reloj: se descarta lo más viejo hasta dejar ~3 bloques
                        let drop = g.len() - want * 3;
                        g.drain(..drop);
                    }
                    for f in 0..FRAMES {
                        out[f * 2 * k + ti * 2] = g.pop_front().unwrap_or(0.0);
                        out[f * 2 * k + ti * 2 + 1] = g.pop_front().unwrap_or(0.0);
                    }
                }
                bytes.clear();
                for s in &out {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                if stdin.write_all(&bytes).is_err() {
                    break;
                }
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else if now - next > Duration::from_millis(500) {
                    next = now; // nos quedamos atrás (equipo suspendido…): no intentar recuperar
                }
            }
        }));
        Mixer { stop, handles, system_failed }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles {
            let _ = h.join();
        }
        if let Ok(mut t) = TRACKS.lock() {
            t.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(pid: u32, ppid: u32, exe: &str) -> ProcEntry {
        ProcEntry { pid, ppid, exe: exe.into() }
    }

    #[test]
    fn raiz_de_un_arbol_de_procesos() {
        let v = vec![e(1, 0, "explorer.exe"), e(10, 1, "chrome.exe"), e(11, 10, "chrome.exe"), e(12, 10, "chrome.exe"), e(20, 1, "Discord.exe"), e(21, 20, "Discord.exe")];
        assert_eq!(root_pid(&v, "CHROME.EXE"), Some(10));
        assert_eq!(root_pid(&v, "discord.exe"), Some(20));
        assert_eq!(root_pid(&v, "spotify.exe"), None);
    }

    #[test]
    fn pistas_segun_la_configuracion() {
        let mut c = Config::default();
        let s = specs_from_config(&c);
        assert_eq!(s.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Sistema", "Juego", "Discord", "Navegador"]);
        c.audio.tracks.mic = true;
        c.audio.tracks.apps[0].enabled = false;
        let s = specs_from_config(&c);
        assert_eq!(s.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Sistema", "Juego", "Navegador", "Micrófono"]);
        c.audio.enabled = false;
        assert!(specs_from_config(&c).is_empty());
    }

    #[test]
    fn argumentos_de_ffmpeg_para_varias_pistas() {
        let specs = vec![
            Spec { title: "Sistema".into(), kind: Kind::System },
            Spec { title: "Juego".into(), kind: Kind::Game },
            Spec { title: "Discord".into(), kind: Kind::App("Discord.exe".into()) },
        ];
        let (input, out) = ffmpeg_audio_args(&specs, 160);
        assert!(input.windows(2).any(|w| w == ["-ac", "6"]));
        let j = out.join(" ");
        assert!(j.contains("asplit=3[s0][s1][s2]"), "{j}");
        assert!(j.contains("[s1]pan=stereo|c0=c2|c1=c3[a1]"), "{j}");
        assert!(j.contains("title=Discord"));
        // una sola pista: sin filtro
        let (input1, out1) = ffmpeg_audio_args(&specs[..1], 160);
        assert!(input1.windows(2).any(|w| w == ["-ac", "2"]));
        assert!(!out1.join(" ").contains("filter_complex"));
    }
}
