# Clipper

Grabador de clips **ligero** para Windows. Mantiene un búfer circular de tu pantalla y de tu audio y, al pulsar un atajo, guarda los últimos segundos
como un MP4. Corre en la bandeja del sistema y se maneja desde una ventana propia: ver clips, recortarlos, comprimirlos para Discord y elegir
qué audio se guarda.

> **Estado: beta (v0.1.0).** Hecho para uso personal y probado en **un solo equipo** (Windows 11, Ryzen 7 5700X, RTX 5060). Funciona bien ahí, pero
> es probable que encuentres cosas que fallan en otros equipos. **Sobre todo buscamos gente con una GPU AMD**: ver [Prueba con AMD](#prueba-con-amd-ayúdanos).

## Qué hace

- **Búfer circular** de 10 s a 10 min (por defecto 60 s) a 1080p/60 o lo que tenga tu monitor. Al pulsar el atajo (**Ctrl + Alt + F9**) se guarda el clip.
- **Audio por pistas**, como Medal: una pista *Sistema* (todo lo que suena) más pistas separadas para *Juego*, *Discord*, el navegador y tu *micrófono*. En el editor
  puedes silenciar o bajar cada una antes de exportar.
- **Editor integrado:** reproducción, recorte, «Rápido» (sin recodificar), «Exacto» y **«Para Discord»** (comprime al tamaño que elijas y copia el archivo al portapapeles).
- **Muy poco consumo** (medido en el equipo de pruebas, grabando 1080p60 con 5 pistas): ~130 MB de RAM y ~1 % de CPU; la codificación la hace la GPU.
- Eliminar clips (van a la Papelera), tema oscuro o claro, inicio con Windows opcional.
- **Sin cuentas, sin nube y sin telemetría.** Todo es local.

## Compatibilidad

| GPU | Codificador | Estado |
|---|---|---|
| **NVIDIA** | NVENC (en la GPU) | **Probado a fondo** |
| **AMD** | AMF (`h264_amf`) | **Experimental: aún no se ha probado en una GPU AMD.** [Ayúdanos a probarlo](#prueba-con-amd-ayúdanos) |
| Intel u otra | `libx264` (CPU) | Probado. Usa bastante más procesador y puede restar FPS al juego |

Con el ajuste por defecto (`Automático`) Clipper mira tu GPU y elige. Si el codificador por hardware no arranca, **pasa solo a CPU** (tanto al grabar como al exportar).

- **Windows 11 recomendado.** Las pistas por aplicación (Juego, Discord, navegador) usan una función de Windows disponible desde la compilación 20348 / Windows 11. En Windows 10 la pista
  *Sistema* y el micrófono deberían funcionar, pero **no se ha probado**.
- La ventana usa **Chrome o Edge** (Edge viene con Windows).
- No probado: portátiles con dos GPU, varios monitores con distinta tarjeta, HDR, juegos en pantalla completa exclusiva.
- Los juegos **en ventana** no se detectan como pista «Juego»: añádelos como una aplicación más en *Configuración → Pistas separadas*.

## Descarga e instalación

1. Descarga el zip del último [release](../../releases) y **extráelo en una carpeta tuya** (por ejemplo `Documentos\Clipper`). No hace falta instalar nada ni ser administrador.
2. Abre `clipper.exe`. Aparece un icono en la bandeja y empieza a grabar. Clic izquierdo en el icono abre la ventana.
3. El zip ya incluye `ffmpeg` (en `bin\`). Si prefieres el tuyo, quita esa carpeta y ten `ffmpeg` en el `PATH`.

> **Aviso de Windows:** `clipper.exe` **no está firmado** (la firma de código cuesta dinero), así que SmartScreen puede mostrar «Windows protegió su PC» (*Más información → Ejecutar de todas formas*) y
> algún antivirus puede marcarlo por error. El código fuente está aquí para que lo revises o lo compiles tú mismo. Cada release publica la **suma SHA-256** del zip.
>
> **Juegos con anticheat (Vanguard, EAC, BattlEye…):** Clipper no se inyecta en los juegos (captura la pantalla con Windows y el audio por proceso), pero **no lo hemos probado con ningún anticheat**.
> Si te preocupa, pruébalo primero con un juego sin anticheat.

## Uso rápido

| Acción | Cómo |
|---|---|
| Guardar los últimos N segundos | Atajo **Ctrl + Alt + F9** (configurable) o el botón *Guardar clip ahora* |
| Abrir la ventana | Clic izquierdo en el icono de la bandeja, o ejecutar `clipper.exe` otra vez |
| Menú de la bandeja | Clic derecho: *Abrir Clipper*, *Guardar clip ahora*, *Salir* |
| Recortar o comprimir un clip | Ventana → **Clips** → clic en el clip |
| Seleccionar varios | Casilla ✓ de la esquina de cada tarjeta, o Ctrl / Mayús + clic |
| Eliminar | Seleccionar → *Eliminar* o tecla Supr (van a la **Papelera**) |
| Cerrar el programa | Icono de la bandeja → *Salir* |

Los clips se guardan en la carpeta `clips\` junto al programa (configurable).

## Pistas de audio

Cada clip lleva la pista **Sistema** (la mezcla de todo lo que suena) y una pista aparte por cada fuente que actives:

| Pista | Qué graba |
|---|---|
| **Sistema** | Todo lo que sale por tu dispositivo de audio. Siempre se graba |
| **Juego** | La aplicación en primer plano **a pantalla completa o sin bordes** (se detecta sola) |
| **Una por aplicación** | Por defecto Discord y el navegador; puedes añadir o quitar más |
| **Micrófono** | **Desactivado por defecto** (grabaría tu voz de forma continua). Con *ganancia* ajustable para micrófonos que entran bajos |

En el editor, **«Mezcla completa»** es la pista Sistema **más tu micrófono** (el micrófono no sale por los altavoces, así que la pista Sistema nunca lo lleva);
**«Mezcla personalizada»** suma solo las pistas que elijas, cada una con su volumen. El recorte rápido conserva todas las pistas; *Exacto* y *Para Discord* dejan un solo audio mezclado.
Cada pista extra suma unos 160 kbps al archivo.

## Iniciar con Windows

Opcional y **desactivado por defecto**: *Configuración → Sistema → Iniciar con Windows*. Crea un único valor en el registro de *tu usuario*
(`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, valor `Clipper`), sin administrador y sin tocar nada del sistema.

**Para quitarlo** (basta una):

1. Desmarcar la opción en *Configuración → Sistema*.
2. *Administrador de tareas → Aplicaciones de inicio → Clipper → Deshabilitar*.
3. En una terminal: `reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v Clipper /f`

Si mueves la carpeta de Clipper, desactiva y vuelve a activar la opción para que apunte a la nueva ruta.

## Configuración

Todo se cambia en la ventana (*Configuración → Guardar y aplicar*, que reinicia la grabación). Se guarda en `config.json`, junto al programa, y también se puede editar a mano.

| Clave | Qué hace | Por defecto |
|---|---|---|
| `buffer_seconds` | Segundos que se conservan (10 a 600) | 60 |
| `fps` | Fotogramas por segundo | 60 |
| `monitor` | Monitor a capturar (0 = principal) | 0 |
| `draw_mouse` | Dibujar el cursor | `true` |
| `encoder` | `auto`, `h264_nvenc`, `hevc_nvenc`, `av1_nvenc`, `h264_amf` (AMD, experimental) o `libx264` (CPU) | `auto` |
| `nvenc_preset` | `p1` (rápido) a `p7` (mejor calidad); solo NVENC | `p5` |
| `bitrate_mbps` / `maxrate_mbps` | Bitrate objetivo y máximo (Mbps) | 15 / 20 |
| `audio.enabled` / `audio.bitrate_kbps` | Grabar audio y su bitrate por pista | `true` / 160 |
| `audio.tracks.game` | Pista «Juego» | `true` |
| `audio.tracks.apps` | Lista de `{ "name", "exe", "enabled" }` con pista propia (máx. 8) | Discord y `chrome.exe` |
| `audio.tracks.mic` / `mic_device` / `mic_gain_db` | Micrófono, su dispositivo (vacío = predeterminado) y ganancia (0 a 30 dB) | `false` / vacío / 0 |
| `hotkey` | Atajo global, p. ej. `ctrl+alt+F9` | `ctrl+alt+F9` |
| `output_dir` | Carpeta de clips (ruta absoluta; vacío = `clips\` junto al programa) | vacío |
| `discord_limit_mb` | Tamaño objetivo del botón *Para Discord* | 9.5 |
| `theme` | `dark` o `light` | `dark` |

Notas: AV1 suele pesar la mitad que H.264 a igual calidad, pero **con el mismo bitrate pesa más**: prueba con la mitad. `Alt + F9` no sirve de atajo si NVIDIA lo usa.

## Privacidad y seguridad

- No hay telemetría ni conexiones a Internet. La ventana habla con un servidor que corre **dentro del propio programa**, solo en `127.0.0.1` (tu equipo), con un token aleatorio en cada arranque.
- El micrófono está **apagado por defecto**. La pista de micrófono solo existe si lo activas tú.
- Los clips y la configuración nunca salen de tu carpeta. Se usa la Papelera de reciclaje, nunca el borrado definitivo, y Clipper **no borra clips por su cuenta**.
- Para copiar el archivo al portapapeles, elegir carpeta y enviar a la Papelera usa PowerShell; «Mostrar en carpeta» abre el Explorador; el inicio con Windows usa `reg.exe`.
  Todo el código está en este repositorio para que lo revises.

## Solución de problemas

Cada evento queda en `clipper.log` (junto al programa).

- **No guarda clips / «Detenida»:** mira el estado en *Inicio* y el final de `clipper.log`. Clipper reinicia solo la grabación si ffmpeg se atasca (por ejemplo tras suspender el equipo).
- **El atajo no responde:** otra aplicación lo tiene registrado o se traga la tecla (Overwolf, NVIDIA Overlay, software del teclado…). Cámbialo en *Configuración*.
  Cada pulsación queda en el log («atajo … pulsado (primer plano: …)»): si no aparece, la tecla no llegó a Clipper.
- **La ventana quedó en blanco tras reiniciar:** ciérrala y ábrela de nuevo (el puerto y el token cambian en cada arranque).
- **Va a tirones o resta FPS:** si usas `libx264` (CPU), baja los FPS o el bitrate, o usa el codificador de tu GPU.

## Prueba con AMD (ayúdanos)

El soporte para GPU AMD está escrito pero **nunca se ha ejecutado en una GPU AMD**: el equipo de desarrollo es NVIDIA. Lo que sí se verificó es que, si el codificador de AMD falla, Clipper cae a CPU y sigue funcionando.
Si tienes una Radeon:

1. Extrae el zip y abre `clipper.exe` con la configuración por defecto (`Automático`).
2. Abre `clipper.log`. Deberías ver una línea como `GPU detectada: AMD Radeon … → codificador h264_amf`.
3. Si **no** aparece `el codificador por hardware (h264_amf) no arranca … paso a libx264`, la grabación está usando tu GPU. Pulsa **Ctrl + Alt + F9** tras unos segundos y abre el clip en la ventana.
4. Comprueba: ¿se ve fluido?, ¿tiene audio?, ¿cuánta CPU/GPU usa (Administrador de tareas)?, ¿funcionan *Recorte exacto* y *Para Discord*?
5. Cuéntalo abriendo un issue con la plantilla **«Prueba con AMD»**, adjuntando el modelo de GPU y la versión del driver, y el `clipper.log` (y `buffer_rs\ffmpeg.log` si falló).

Aunque todo salga mal, tu informe nos sirve: dice exactamente qué hay que corregir.

## Compilar desde el código fuente

Requiere Rust con la toolchain `stable-x86_64-pc-windows-gnu` y un MinGW-w64 (por ejemplo WinLibs).

```
cargo build --release
```

El ejecutable queda en `target\release\clipper.exe`. Para ejecutarlo hace falta un `ffmpeg` (en `bin\ffmpeg.exe` junto al exe, o en el `PATH`). Útil para depurar:
`clipper.exe --ui` (solo la interfaz) y `clipper.exe --selftest 15` (graba 15 s, guarda un clip y sale).

## Licencia

[MIT](LICENSE). El zip del release incluye un ffmpeg de terceros bajo **GPL v3**; ver [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
