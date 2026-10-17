// Grabador de clips en segundo plano.
// ffmpeg (ddagrab + NVENC) escribe segmentos cortos en un búfer circular; el audio del sistema se
// captura con WASAPI loopback (cpal) y se pasa a ffmpeg por stdin. Al pulsar el atajo (o usar el
// menú de la bandeja) se unen los últimos N segundos en un MP4 sin recodificar.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde::{Deserialize, Serialize};

mod audio;
mod encoding;
mod webui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Diagnostics::Debug::Beep;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Una aplicación con su propia pista de audio (captura por proceso).
#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(default)]
struct AppTrack {
    name: String,
    exe: String,
    enabled: bool,
}

impl Default for AppTrack {
    fn default() -> Self {
        Self { name: String::new(), exe: String::new(), enabled: true }
    }
}

/// Pistas de audio además de «Sistema» (la mezcla completa, que siempre se graba).
#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(default)]
struct TracksCfg {
    /// Pista «Juego»: la aplicación a pantalla completa en primer plano.
    game: bool,
    /// Pista «Micrófono» (desactivada por defecto: grabaría tu voz de forma continua).
    mic: bool,
    /// Nombre del micrófono; vacío = el predeterminado.
    mic_device: String,
    /// Ganancia del micrófono en dB (0 a 30): muchos micrófonos USB entran muy bajos.
    mic_gain_db: f32,
    apps: Vec<AppTrack>,
}

impl Default for TracksCfg {
    fn default() -> Self {
        Self {
            game: true,
            mic: false,
            mic_device: String::new(),
            mic_gain_db: 0.0,
            apps: vec![
                AppTrack { name: "Discord".into(), exe: "Discord.exe".into(), enabled: true },
                AppTrack { name: "Navegador".into(), exe: "chrome.exe".into(), enabled: true },
            ],
        }
    }
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
struct AudioCfg {
    enabled: bool,
    bitrate_kbps: u32,
    tracks: TracksCfg,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self { enabled: true, bitrate_kbps: 160, tracks: TracksCfg::default() }
    }
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
struct Config {
    buffer_seconds: u32,
    segment_seconds: u32,
    fps: u32,
    monitor: u32,
    draw_mouse: bool,
    encoder: String,
    bitrate_mbps: u32,
    maxrate_mbps: u32,
    nvenc_preset: String,
    audio: AudioCfg,
    hotkey: String,
    beep_on_save: bool,
    output_dir: String,
    ffmpeg_path: String,
    /// "dark" (estudio oscuro, por defecto) o "light" (claro y limpio). Solo afecta a la ventana de configuración.
    theme: String,
    /// Tamaño objetivo (MB) del botón «Para Discord» del editor.
    discord_limit_mb: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buffer_seconds: 60,
            segment_seconds: 2,
            fps: 60,
            monitor: 0,
            draw_mouse: true,
            encoder: "auto".into(), // según la GPU: NVIDIA → NVENC, AMD → AMF (experimental), otra → CPU
            bitrate_mbps: 15,
            maxrate_mbps: 20,
            nvenc_preset: "p5".into(),
            audio: AudioCfg::default(),
            hotkey: "ctrl+alt+F9".into(), // Alt+F9 ya lo usa NVIDIA (ShadowPlay)
            beep_on_save: true,
            output_dir: String::new(),
            ffmpeg_path: "ffmpeg".into(),
            theme: "dark".into(),
            discord_limit_mb: 9.5,
        }
    }
}

struct Paths {
    buf_dir: PathBuf,
    out_dir: PathBuf,
    ffmpeg: PathBuf,
    log: PathBuf,
}

fn log(path: &Path, msg: &str) {
    webui::push_event(msg); // los mensajes útiles se ven también en «Actividad»
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_config(dir: &Path) -> (Config, Option<String>) {
    match fs::read_to_string(dir.join("config.json")) {
        // El Bloc de notas y PowerShell 5.1 pueden guardar con BOM; serde_json no lo admite.
        Ok(text) => match serde_json::from_str::<Config>(text.trim_start_matches('\u{feff}')) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("config.json inválido, uso valores por defecto: {e}"))),
        },
        Err(_) => (Config::default(), Some("no hay config.json, uso valores por defecto".into())),
    }
}

fn resolve_paths(cfg: &Config, dir: &Path) -> Paths {
    let bundled = dir.join("bin").join("ffmpeg.exe");
    let ffmpeg = if cfg.ffmpeg_path == "ffmpeg" && bundled.exists() {
        bundled
    } else {
        PathBuf::from(&cfg.ffmpeg_path)
    };
    let out_dir = if cfg.output_dir.is_empty() { dir.join("clips") } else { PathBuf::from(&cfg.output_dir) };
    Paths { buf_dir: dir.join("buffer_rs"), log: dir.join("clipper.log"), out_dir, ffmpeg }
}

fn silent_cmd(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

struct Recorder {
    child: Child,
    /// Captura y mezcla de las pistas de audio (None si el audio está desactivado).
    mixer: Option<audio::Mixer>,
    job: Option<HANDLE>,
    started: Instant,
    buf_dir: PathBuf,
    seg_secs: u32,
}

/// Job Object que mata a ffmpeg si este programa termina por cualquier causa (evita huérfanos).
fn kill_on_close_job(child: &Child) -> Option<HANDLE> {
    use std::os::windows::io::AsRawHandle;
    unsafe {
        let job = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .is_ok()
            && AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).is_ok();
        if ok {
            Some(job)
        } else {
            let _ = CloseHandle(job);
            None
        }
    }
}

impl Recorder {
    fn start(cfg: &Config, paths: &Paths) -> Result<Self, String> {
        let _ = fs::remove_dir_all(&paths.buf_dir);
        fs::create_dir_all(&paths.buf_dir).map_err(|e| format!("no pude crear el búfer: {e}"))?;
        fs::create_dir_all(&paths.out_dir).map_err(|e| format!("no pude crear la carpeta de clips: {e}"))?;

        let nseg = ((cfg.buffer_seconds as f32 / cfg.segment_seconds as f32).ceil() as u32 + 2).max(3);
        let gop = cfg.fps * cfg.segment_seconds;
        let mouse = if cfg.draw_mouse { 1 } else { 0 };

        // Audio: una pista «Sistema» (mezcla completa) más las de juego, aplicaciones y micrófono.
        let specs = audio::specs_from_config(cfg);
        let (aud_in, aud_out) = if specs.is_empty() { (Vec::new(), Vec::new()) } else { audio::ffmpeg_audio_args(&specs, cfg.audio.bitrate_kbps) };

        let enc = encoding::resolve(&cfg.encoder);
        if cfg.encoder == "auto" {
            log(&paths.log, &encoding::detection_note());
        }
        let mut args: Vec<String> = vec![
            "-hide_banner".into(), "-loglevel".into(), "warning".into(), "-y".into(),
            "-f".into(), "lavfi".into(), "-i".into(),
            format!("ddagrab=output_idx={}:framerate={}:draw_mouse={}", cfg.monitor, cfg.fps, mouse),
        ];
        args.extend(aud_in);
        args.extend(["-map", "0:v"].map(String::from));
        args.extend(encoding::capture_args(&enc, cfg, gop)); // incluye -g
        args.extend(aud_out); // división en pistas, AAC por pista y sus nombres
        args.extend([
            "-f".to_string(), "segment".into(),
            "-segment_time".into(), cfg.segment_seconds.to_string(),
            "-segment_wrap".into(), nseg.to_string(),
            "-reset_timestamps".into(), "1".into(),
            // Matroska admite todos los codecs (AV1 incluido) y tolera mejor un corte inesperado que MPEG-TS.
            "-segment_format".into(), "matroska".into(),
            paths.buf_dir.join("seg%03d.mkv").to_string_lossy().into_owned(),
        ]);
        log(&paths.log, &format!("ffmpeg {}", args.join(" ")));

        let ff_log = File::create(paths.buf_dir.join("ffmpeg.log")).map_err(|e| e.to_string())?;
        let mut cmd = silent_cmd(&paths.ffmpeg);
        cmd.args(&args).stdout(Stdio::null()).stderr(ff_log);
        cmd.stdin(if specs.is_empty() { Stdio::null() } else { Stdio::piped() });
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("no pude lanzar ffmpeg ({}): {e}", paths.ffmpeg.display()))?;

        let job = kill_on_close_job(&child);
        if job.is_none() {
            log(&paths.log, "aviso: no pude crear el Job Object; ffmpeg podría quedar huérfano si me cierran a la fuerza");
        }
        let mixer = if specs.is_empty() {
            None
        } else {
            let stdin = child.stdin.take().ok_or("ffmpeg sin stdin")?;
            log(
                &paths.log,
                &format!("audio: {} pistas ({})", specs.len(), specs.iter().map(|s| s.title.as_str()).collect::<Vec<_>>().join(", ")),
            );
            Some(audio::Mixer::start(&specs, stdin, cfg, Arc::new(paths.log.clone())))
        };
        Ok(Self {
            child,
            mixer,
            job,
            started: Instant::now(),
            buf_dir: paths.buf_dir.clone(),
            seg_secs: cfg.segment_seconds,
        })
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Motivo por el que la grabación no está sana aunque ffmpeg siga vivo, si lo hay: audio caído o
    /// ningún segmento nuevo en un rato (tras una suspensión ffmpeg puede quedarse colgado sin morir).
    fn unhealthy(&self) -> Option<String> {
        if self.mixer.as_ref().map(|m| m.system_failed.load(Ordering::Relaxed)).unwrap_or(false) {
            return Some("falló el dispositivo de audio".into());
        }
        if self.started.elapsed() < Duration::from_secs(20) {
            return None;
        }
        let limit = Duration::from_secs((self.seg_secs as u64 * 3).max(10));
        let newest = fs::read_dir(&self.buf_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".mkv"))
            .filter_map(|e| e.metadata().ok()?.modified().ok())
            .max();
        match newest {
            Some(t) => match std::time::SystemTime::now().duration_since(t) {
                Ok(age) if age > limit => Some(format!("sin segmentos nuevos desde hace {} s", age.as_secs())),
                _ => None,
            },
            None => Some("no hay segmentos en el búfer".into()),
        }
    }

    fn stop(mut self) {
        // Matar a ffmpeg primero: si está colgado, el hilo que le escribe el audio queda bloqueado en su
        // tubería y esperar a ese hilo congelaría también la vigilancia. Los segmentos ya son archivos
        // independientes, así que no hace falta un cierre ordenado.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(m) = self.mixer.take() {
            m.stop();
        }
        if let Some(job) = self.job.take() {
            unsafe {
                let _ = CloseHandle(job);
            }
        }
    }
}

fn join_and_cut(cfg: &Config, paths: &Paths) -> Result<PathBuf, String> {
    let mut segs: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&paths.buf_dir)
        .map_err(|_| "la grabación no está activa, así que no hay búfer que guardar".to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("seg") && n.ends_with(".mkv")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    segs.sort_by_key(|s| s.0);
    if segs.len() < 2 {
        return Err("aún no hay suficiente búfer".into());
    }
    if segs.len() > 2 {
        segs.pop(); // el último aún se está escribiendo
    }
    // Cada segmento empieza en un fotograma clave (gop = segmento): cortar por segmentos completos
    // mantiene vídeo y audio alineados; recortar con -sseof dejaba ~2 s de audio sin imagen al inicio.
    let n = ((cfg.buffer_seconds as f32 / cfg.segment_seconds as f32).ceil() as usize).max(1);
    if segs.len() > n {
        segs.drain(..segs.len() - n);
    }
    let tmp = std::env::temp_dir().join(format!("clipper_{}", chrono::Local::now().timestamp_millis()));
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = (|| -> Result<PathBuf, String> {
        let mut list = String::new();
        for (i, (_, p)) in segs.iter().enumerate() {
            let dst = tmp.join(format!("{i:04}.mkv"));
            fs::copy(p, &dst).map_err(|e| e.to_string())?;
            // La duración real de un segmento es la del VÍDEO (gop = fps × segmento = segment_seconds exactos, a fps
            // constante). Sin esta línea ffmpeg usa la duración del contenedor, que incluye el audio (empieza 20-40 ms
            // tarde y termina después), y deja un hueco de 20-40 ms en el vídeo en cada unión: un tirón cada 2 s.
            list.push_str(&format!("file '{}'\nduration {}\n", dst.to_string_lossy().replace('\\', "/"), cfg.segment_seconds));
        }
        let list_path = tmp.join("list.txt");
        fs::write(&list_path, list).map_err(|e| e.to_string())?;
        let out = paths.out_dir.join(chrono::Local::now().format("clip_%Y%m%d_%H%M%S.mp4").to_string());
        let ok = silent_cmd(&paths.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list_path)
            // -map 0: conserva el vídeo y TODAS las pistas de audio (por defecto ffmpeg elegiría solo una).
            .args(["-map", "0", "-c", "copy", "-movflags", "+faststart"])
            .arg(&out)
            .status()
            .map_err(|e| e.to_string())?
            .success();
        if ok { Ok(out) } else { Err("ffmpeg falló al crear el MP4".into()) }
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

fn save_clip(cfg: Config, paths: Arc<Paths>) -> Result<PathBuf, String> {
    match join_and_cut(&cfg, &paths) {
        Ok(p) => {
            log(&paths.log, &format!("clip guardado: {}", p.display()));
            if cfg.beep_on_save {
                unsafe {
                    let _ = Beep(1000, 150);
                }
            }
            Ok(p)
        }
        Err(e) => {
            log(&paths.log, &format!("no se pudo guardar el clip: {e}"));
            Err(e)
        }
    }
}

fn make_icon() -> Icon {
    // Círculo rojo de 32x32 (RGBA).
    let (w, h) = (32u32, 32u32);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = dx * dx + dy * dy <= 14.0 * 14.0;
            px.extend_from_slice(if inside { &[220, 40, 40, 255] } else { &[0, 0, 0, 0] });
        }
    }
    Icon::from_rgba(px, w, h).expect("icono")
}

/// Convierte el texto del atajo; si es inválido usa ctrl+alt+F9 y lo deja anotado en el log.
fn parse_hotkey(cfg: &Config, paths: &Paths) -> HotKey {
    match cfg.hotkey.parse() {
        Ok(h) => h,
        Err(e) => {
            log(&paths.log, &format!("atajo inválido '{}': {e}; uso ctrl+alt+F9", cfg.hotkey));
            "ctrl+alt+F9".parse().unwrap()
        }
    }
}

/// Segundos de búfer disponibles ahora (segmentos completos × duración), sin pasar del objetivo.
fn buffered_secs(paths: &Paths, cfg: &Config) -> u32 {
    let n = fs::read_dir(&paths.buf_dir)
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".mkv")).count())
        .unwrap_or(0);
    ((n.saturating_sub(1)) as u32 * cfg.segment_seconds).min(cfg.buffer_seconds)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    // Modo de pruebas: solo el servidor de la interfaz, sin bandeja ni grabación.
    if has("--ui") {
        let dir = exe_dir();
        let (cfg, _) = load_config(&dir);
        let paths = resolve_paths(&cfg, &dir);
        let Some(ui) = webui::start(dir.clone(), &paths.log) else { return };
        if has("--no-browser") {
            let _ = fs::write(dir.join("ui_url.txt"), ui.url());
        } else {
            webui::open_window(&ui, None);
        }
        let t0 = Instant::now();
        while !webui::QUIT.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(600) {
            thread::sleep(Duration::from_millis(300));
        }
        let _ = fs::remove_file(dir.join("ui_url.txt"));
        return;
    }

    // Prueba sin teclado ni bandeja: graba N segundos, guarda un clip y sale (código 0 si salió bien).
    // Pensada para verificar el pipeline desde una copia del .exe con su propio config.json.
    if let Some(pos) = args.iter().position(|a| a == "--selftest") {
        let secs: u64 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(15);
        let dir = exe_dir();
        let (cfg, _) = load_config(&dir);
        let paths = Arc::new(resolve_paths(&cfg, &dir));
        let code = match Recorder::start(&cfg, &paths) {
            Ok(rec) => {
                thread::sleep(Duration::from_secs(secs));
                let res = join_and_cut(&cfg, &paths);
                rec.stop();
                match res {
                    Ok(p) => {
                        log(&paths.log, &format!("selftest: clip guardado {}", p.display()));
                        0
                    }
                    Err(e) => {
                        log(&paths.log, &format!("selftest: falló: {e}"));
                        1
                    }
                }
            }
            Err(e) => {
                log(&paths.log, &format!("selftest: no pude iniciar: {e}"));
                1
            }
        };
        std::process::exit(code);
    }

    let dir = exe_dir();

    // Instancia única. Si ya hay una, esta ejecución solo abre la ventana de la que está viva.
    unsafe {
        let _ = CreateMutexW(None, true, w!("Local\\ClipperRecorderRs"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(url) = fs::read_to_string(dir.join("ui_live.txt")) {
                webui::open_url(&dir, url.trim());
            }
            return;
        }
    }

    let (mut cfg, cfg_warn) = load_config(&dir);
    let mut paths = Arc::new(resolve_paths(&cfg, &dir));
    log(&paths.log, "=== clipper iniciado ===");
    if let Some(w) = cfg_warn {
        log(&paths.log, &w);
    }

    // Interfaz web local: la dirección (con su token) queda en ui_live.txt para abrirla desde otra ejecución.
    let ui = webui::start(dir.clone(), &paths.log);
    if let Some(u) = &ui {
        let _ = fs::write(dir.join("ui_live.txt"), u.url());
    }

    // Atajo global.
    let manager = GlobalHotKeyManager::new().expect("no pude crear el gestor de atajos");
    let mut hotkey: HotKey = parse_hotkey(&cfg, &paths);
    if let Err(e) = manager.register(hotkey) {
        log(&paths.log, &format!("no pude registrar el atajo (¿en uso por otra app?): {e}"));
    }

    // Bandeja: clic izquierdo abre Clipper; clic derecho, un menú corto.
    let menu = Menu::new();
    let open_item = MenuItem::new("Abrir Clipper", true, None);
    let save_item = MenuItem::new(format!("Guardar clip ahora ({})", cfg.hotkey), true, None);
    let quit_item = MenuItem::new("Salir", true, None);
    let _ = menu.append_items(&[&open_item, &save_item, &PredefinedMenuItem::separator(), &quit_item]);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(make_icon())
        .with_tooltip("Clipper: iniciando…")
        .build()
        .expect("no pude crear el icono de bandeja");

    let mut rec: Option<Recorder> = None;
    let mut last_try = Instant::now() - Duration::from_secs(60);
    let mut hk_id = hotkey.id();
    let mut last_health = Instant::now();
    let mut last_metrics = Instant::now() - Duration::from_secs(10);
    let mut last_open = Instant::now() - Duration::from_secs(10);
    let mut fail_count = 0u32;
    // Si NVENC no arranca (PC sin GPU NVIDIA o sin su controlador), se pasa solo a libx264 (CPU).
    let mut quick_deaths = 0u32;
    let mut use_cpu = false;
    webui::set_state("restarting");

    let open_ui = |last_open: &mut Instant| {
        if last_open.elapsed() > Duration::from_secs(2) {
            *last_open = Instant::now();
            if let Some(u) = &ui {
                webui::open_window(u, None);
            }
        }
    };

    // Al abrir clipper.exe a mano se abre la ventana (si no, solo se vería un icono en la bandeja y parecería que no
    // pasa nada). El inicio con Windows lo arranca con --background: ahí empieza a grabar sin abrir nada.
    if !has(webui::BACKGROUND_ARG) {
        open_ui(&mut last_open);
    }
    thread::spawn(webui::migrate_autostart); // inicio con Windows ya activado por una versión anterior

    'main: loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'main;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                open_ui(&mut last_open);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *open_item.id() {
                open_ui(&mut last_open);
            } else if ev.id == *save_item.id() {
                log(&paths.log, "guardar clip pedido desde la bandeja");
                let (c, p) = (cfg.clone(), paths.clone());
                thread::spawn(move || {
                    let _ = save_clip(c, p);
                });
            } else if ev.id == *quit_item.id() {
                break 'main;
            }
        }

        // La interfaz guardó la configuración: recargar config, atajo y grabación.
        if webui::RELOAD.swap(false, Ordering::Relaxed) {
            let (new_cfg, warn) = load_config(&dir);
            if let Some(w) = warn {
                log(&paths.log, &w);
            }
            cfg = new_cfg;
            paths = Arc::new(resolve_paths(&cfg, &dir));
            let _ = manager.unregister(hotkey);
            hotkey = parse_hotkey(&cfg, &paths);
            hk_id = hotkey.id();
            if let Err(e) = manager.register(hotkey) {
                log(&paths.log, &format!("no pude registrar el atajo (¿en uso por otra app?): {e}"));
            }
            save_item.set_text(format!("Guardar clip ahora ({})", cfg.hotkey));
            if let Some(r) = rec.take() {
                r.stop();
            }
            fail_count = 0;
            quick_deaths = 0;
            use_cpu = false;
            webui::set_state("restarting");
            last_try = Instant::now() - Duration::from_secs(60); // reinicia la grabación ya
            log(&paths.log, "configuración recargada");
        }
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.id == hk_id && ev.state == HotKeyState::Pressed {
                log(&paths.log, &format!("atajo {} pulsado (primer plano: {})", cfg.hotkey, audio::foreground_exe()));
                let (c, p) = (cfg.clone(), paths.clone());
                thread::spawn(move || {
                    let _ = save_clip(c, p);
                });
            }
        }

        // Vigilancia: (re)lanzar la grabación si ffmpeg murió o si está atascado (cada 2 s se comprueba
        // que sigan llegando segmentos; p. ej. tras suspender el equipo o cambiar de auriculares).
        let mut running = rec.as_mut().map(|r| r.alive()).unwrap_or(false);
        if running && last_health.elapsed() > Duration::from_secs(2) {
            last_health = Instant::now();
            if let Some(reason) = rec.as_ref().and_then(|r| r.unhealthy()) {
                log(&paths.log, &format!("grabación atascada ({reason}); la reinicio"));
                if let Some(old) = rec.take() {
                    old.stop();
                }
                running = false;
                webui::set_state("restarting");
                last_try = Instant::now() - Duration::from_secs(60);
            }
        }
        if !running && last_try.elapsed() > Duration::from_secs(10) {
            last_try = Instant::now();
            if let Some(old) = rec.take() {
                log(&paths.log, "ffmpeg se detuvo; reintento");
                quick_deaths = if old.started.elapsed() < Duration::from_secs(20) { quick_deaths + 1 } else { 0 };
                old.stop();
            }
            if quick_deaths >= 2 && !use_cpu && encoding::is_hardware(&encoding::resolve(&cfg.encoder)) {
                use_cpu = true;
                log(&paths.log, &format!("el codificador por hardware ({}) no arranca (¿GPU sin soporte o sin controlador?): paso a libx264 (CPU). Se puede elegir otro en Configuración", encoding::resolve(&cfg.encoder)));
            }
            let start_cfg = if use_cpu {
                let mut c = cfg.clone();
                c.encoder = "libx264".into();
                c
            } else {
                cfg.clone()
            };
            match Recorder::start(&start_cfg, &paths) {
                Ok(r) => {
                    rec = Some(r);
                    fail_count = 0;
                    let _ = tray.set_tooltip(Some(format!("Clipper: grabando (últimos {} s, {})", cfg.buffer_seconds, cfg.hotkey)));
                }
                Err(e) => {
                    fail_count += 1;
                    log(&paths.log, &format!("no pude iniciar la grabación: {e}"));
                    let _ = tray.set_tooltip(Some("Clipper: detenido (ver clipper.log)"));
                }
            }
        }

        // Estado y métricas para la interfaz.
        let alive_now = rec.as_mut().map(|r| r.alive()).unwrap_or(false);
        webui::set_state(if alive_now { "recording" } else if fail_count >= 3 { "stopped" } else { "restarting" });
        if last_metrics.elapsed() > Duration::from_secs(2) {
            last_metrics = Instant::now();
            let me = webui::working_set_mb(std::process::id()).unwrap_or(0.0);
            let ff = rec.as_ref().and_then(|r| webui::working_set_mb(r.pid())).unwrap_or(0.0);
            webui::set_metrics(me + ff, buffered_secs(&paths, &cfg));
        }
        thread::sleep(Duration::from_millis(30));
    }

    if let Some(r) = rec.take() {
        r.stop();
    }
    let _ = fs::remove_file(dir.join("ui_live.txt"));
    log(&paths.log, "=== clipper cerrado ===");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atajos_validos() {
        for s in ["ctrl+alt+F9", "alt+F9", "ctrl+shift+A", "alt+5", "ctrl+Space", "ctrl+Up", "F8"] {
            assert!(s.parse::<HotKey>().is_ok(), "no parsea: {s}");
        }
    }

    #[test]
    fn config_con_bom_se_lee() {
        let json = "\u{feff}{\"buffer_seconds\": 30, \"hotkey\": \"ctrl+alt+F9\"}";
        let dir = std::env::temp_dir().join("clipper_test_cfg");
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("config.json"), json).unwrap();
        let (cfg, warn) = load_config(&dir);
        assert!(warn.is_none());
        assert_eq!(cfg.buffer_seconds, 30);
        assert_eq!(cfg.fps, 60); // por defecto
        let _ = fs::remove_dir_all(&dir);
    }
}
