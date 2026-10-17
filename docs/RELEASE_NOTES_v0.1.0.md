# Clipper v0.1.0 (beta)

Primera versión pública de Clipper: un grabador de clips **ligero** para Windows (búfer circular, audio por pistas, editor y «Para Discord»).

## Cómo probarlo
1. Descarga `Clipper-0.1.0-win64.zip` y extráelo en una carpeta tuya (por ejemplo `Documentos\Clipper`).
2. Abre `clipper.exe`: aparece en la bandeja y graba. **Ctrl + Alt + F9** guarda los últimos 60 s. Clic izquierdo en el icono abre la ventana.
3. Compara el `SHA-256` del zip con el publicado abajo.

> `clipper.exe` no está firmado: SmartScreen o tu antivirus pueden avisar. El código está en el repositorio. Tampoco se ha probado con juegos con anticheat.

## Estado
- **NVIDIA (NVENC): probado a fondo** en un equipo (Windows 11, RTX 5060).
- **AMD (AMF): experimental, nunca probado en una GPU AMD.** → **¡Buscamos testers con Radeon!** Abre un issue con la plantilla «Prueba con AMD».
- Intel y otras GPU: usan `libx264` (CPU), probado pero más pesado.
- Windows 10: las pistas por aplicación probablemente no funcionen (requieren Windows 11); sin probar.

## Novedades
- Búfer circular con atajo global, bandeja del sistema y ventana de gestión (Inicio, Clips, Recortar, Configuración).
- Audio por pistas: Sistema, Juego, Discord, navegador y micrófono (con ganancia), con mezclador en el editor.
- Recorte rápido, exacto y «Para Discord» (comprime al tamaño elegido y copia el archivo).
- Codificador automático según la GPU, con caída a CPU si el de hardware falla.
- Inicio con Windows opcional (con instrucciones para quitarlo) y borrado a la Papelera.

## Contenido del zip
`clipper.exe`, `bin\ffmpeg.exe` (compilación de Gyan, **GPL v3**; licencia y enlace al código en `ffmpeg-licencia\`), `LICENSE` (MIT del programa), `THIRD_PARTY_NOTICES.md` y `LEEME.md`.

## Problemas conocidos
- Los juegos en ventana no se detectan como pista «Juego» (añádelos como aplicación).
- Portátiles con dos GPU, HDR y pantalla completa exclusiva: sin probar.
- La ventana usa Chrome/Edge y consume ~0,5 GB de RAM mientras está abierta (el grabador en sí usa ~130 MB).
