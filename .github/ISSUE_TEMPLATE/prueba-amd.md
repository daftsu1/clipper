---
name: Prueba con AMD
about: Cuéntanos cómo funcionó Clipper en tu GPU AMD (aunque haya fallado)
title: "[AMD] "
labels: amd, prueba
---

**Gracias por probarlo.** El soporte para AMD nunca se ha ejecutado en una GPU AMD; cualquier resultado, bueno o malo, es útil.

## Tu equipo
- GPU (modelo exacto, p. ej. Radeon RX 6700 XT):
- Versión del driver (Adrenalin) :
- Windows (10 u 11, versión):
- Procesador:
- ¿Portátil con dos GPU (Intel/AMD + otra)? sí / no

## Qué pasó
- Línea de `clipper.log` que empieza por `GPU detectada:` (cópiala aquí):
- ¿Aparece `el codificador por hardware (h264_amf) no arranca … paso a libx264`?  sí / no
  (si **sí**, Clipper estaba grabando por CPU y la prueba de AMD no llegó a ejecutarse)
- ¿Se guardó el clip con el atajo (Ctrl + Alt + F9)?  sí / no
- ¿El vídeo se ve fluido, sin tirones ni imagen verde/rota?
- ¿Tiene audio? ¿Se ven las pistas en el editor?
- Consumo mientras graba (Administrador de tareas → CPU y GPU, con y sin juego abierto):
- ¿Funcionan *Recorte exacto* y *Para Discord*?
- FPS del juego con Clipper grabando frente a sin grabar, si lo mediste:

## Archivos
Adjunta `clipper.log` y, si falló, `buffer_rs\ffmpeg.log` (están junto a `clipper.exe`). Revisa que no contengan nada que no quieras compartir (son solo mensajes del programa y de ffmpeg).

## Cualquier otra cosa
