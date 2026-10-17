# Avisos de terceros

Clipper se distribuye bajo la licencia MIT (ver `LICENSE`). Usa y/o acompaña a los siguientes componentes de terceros.

## FFmpeg (se distribuye junto al ejecutable, no dentro del repositorio)

El **zip del release** incluye `bin\ffmpeg.exe`, una compilación de [FFmpeg](https://ffmpeg.org) hecha por [www.gyan.dev](https://www.gyan.dev/ffmpeg/builds/) (versión `9.0.2-full_build`).
Esa compilación está licenciada bajo **GNU GPL versión 3** (se activó `--enable-gpl --enable-version3`).

- Clipper **no enlaza** con FFmpeg: lo ejecuta como un programa independiente (un proceso aparte), por lo que Clipper conserva su licencia MIT. FFmpeg conserva la suya.
- El texto completo de la GPL v3 y el README de esa compilación están en la carpeta `ffmpeg-licencia\` del zip.
- **Código fuente** de esa compilación de FFmpeg: <https://github.com/FFmpeg/FFmpeg/commit/946fcce07b> (más información y los scripts de compilación en <https://www.gyan.dev/ffmpeg/builds/>). Si quieres recibir el código fuente, abre un issue en este repositorio.
- Puedes sustituir `bin\ffmpeg.exe` por cualquier otro ffmpeg reciente que incluya `ddagrab`, `libx264` y el codificador de tu GPU; Clipper también lo busca en el `PATH` si no existe `bin\ffmpeg.exe`.

## Bibliotecas de Rust enlazadas en `clipper.exe`

Todas con licencias permisivas (MIT, Apache-2.0, Unlicense, Unicode). Lista generada con `cargo metadata` para Windows (52 paquetes):

| Paquete | Versión | Licencia |
|---|---|---|
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| chrono | 0.4.45 | MIT OR Apache-2.0 |
| crossbeam-channel | 0.5.17 | MIT OR Apache-2.0 |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 |
| dpi | 0.1.2 | Apache-2.0 AND MIT |
| global-hotkey | 0.6.4 | Apache-2.0 OR MIT |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| keyboard-types | 0.7.0 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| muda | 0.15.3 | Apache-2.0 OR MIT |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.9 | MIT OR Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror | 1.0.69 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 |
| tray-icon | 0.19.3 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 |
| wasapi | 0.26.0 | MIT |
| windows | 0.58.0 | MIT OR Apache-2.0 |
| windows | 0.62.2 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 |
| windows-core | 0.58.0 | MIT OR Apache-2.0 |
| windows-core | 0.62.2 | MIT OR Apache-2.0 |
| windows-future | 0.3.2 | MIT OR Apache-2.0 |
| windows-implement | 0.58.0 | MIT OR Apache-2.0 |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 |
| windows-interface | 0.58.0 | MIT OR Apache-2.0 |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 |
| windows-result | 0.2.0 | MIT OR Apache-2.0 |
| windows-result | 0.4.1 | MIT OR Apache-2.0 |
| windows-strings | 0.1.0 | MIT OR Apache-2.0 |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 |
| windows-sys | 0.59.0 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 |
| zmij | 1.0.23 | MIT |

Los textos de estas licencias están en el código fuente de cada paquete en <https://crates.io>.
