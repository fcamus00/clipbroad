# clipbroad: diseño

Compartir el portapapeles entre las máquinas propias (Linux/Hyprland, macOS, Windows).
Inspirado en [ClipCascade](https://github.com/Sathvik-Rao/ClipCascade), pero en Rust y sin servidor.

Este documento registra las **decisiones** y su porqué. Si una decisión cambia, se actualiza aquí en el
mismo PR que la cambia.

## Alcance

- **Plataformas:** Linux (Wayland, Hyprland como objetivo principal), macOS y Windows.
- **Red:** misma LAN; opcionalmente a través de Tailscale. No se contempla internet abierto sin Tailscale.
- **Contenido:** MVP solo texto. Después, imágenes y luego archivos.
- **Fuera de alcance (indefinidamente):** móvil (Android/iOS), sincronización con terceros, cuentas de usuario.

## Arquitectura

**P2P en malla completa, sin servidor.** El mismo binario `clipbroad` corre como daemon en cada máquina y
se conecta directo con cada peer emparejado.

Por qué no un servidor central (como ClipCascade):
- Un salto en vez de dos (más rápido, en especial para imágenes y archivos).
- Sin punto único de falla: si una máquina está apagada, las demás siguen sincronizando.
- Un solo artefacto que construir y desplegar.

### Transporte: iroh

[`iroh`](https://github.com/n0-computer/iroh) 1.x (QUIC). Lo que aporta:
- Identidad por clave pública Ed25519 (`EndpointId`); la `SecretKey` se persiste en disco para que la
  identidad sea estable.
- Cifrado y autenticación de peers (TLS sobre QUIC); no escribimos criptografía propia.
- Conexiones directas por LAN o por Tailscale.

Configuración decidida:
- **Preset `presets::Minimal`**: sin relays de n0 ni descubrimiento DNS/pkarr. Ningún tráfico sale a
  servicios de terceros.
- **Descubrimiento en LAN** con un crate de mDNS (`iroh-mdns-address-lookup` o `iroh-mdns-peer-lookup`;
  los formatos son incompatibles entre sí, así que se elige uno y todos los nodos usan el mismo).
- **Tailscale**: mDNS no cruza la tailnet, así que cada peer puede tener además direcciones estáticas en la
  configuración (`EndpointAddr::new(id).with_ip_addr(...)`), por ejemplo `100.x.y.z:puerto` o el
  hostname de MagicDNS resuelto. Puerto de escucha fijo y configurable.
- **Allowlist**: solo se aceptan conexiones de `EndpointId` emparejados, usando `EndpointHooks`
  (ver el ejemplo `auth-hook` de iroh). Todo lo demás se rechaza.

### Emparejamiento

El usuario corre `clipbroad pair` en una máquina, que muestra un ticket (EndpointId + direcciones + un
secreto de un solo uso), y `clipbroad pair <ticket>` en la otra. Ambas guardan el `EndpointId` de la
otra en su configuración. Los detalles se cierran en la fase 2.

### Protocolo

- Un ALPN propio (`clipbroad/1`). Mensajes serializados con `serde` + `postcard`, enmarcados con
  prefijo de longitud.
- Mensaje principal `Clip`: id, origen (`EndpointId`), timestamp, tipo de contenido, MIME, tamaño,
  hash BLAKE3 y payload.
- El payload va **inline** si es pequeño (umbral configurable; texto e imágenes típicas). Para archivos
  grandes se anuncia el metadato y el contenido se transfiere aparte (ver "Archivos").
- `PROTOCOL_VERSION` se intercambia al conectar; versiones incompatibles se rechazan con un error claro.
- El protocolo se diseña desde el MVP con los campos de tipo/MIME/tamaño/hash, para que agregar imágenes y
  archivos no lo rompa.

### Semántica de sincronización

- **Último en escribir gana.** Cada copia local nueva se envía a todos los peers; al recibir, se escribe
  en el portapapeles local.
- **Anti-eco:** al escribir contenido remoto en el portapapeles local, el SO dispara un evento de cambio.
  Se guarda el hash del último contenido aplicado desde la red y se ignora el evento local que coincide.
- **Contenido sensible: nunca se sincroniza** (ver la sección siguiente).

## Portapapeles por plataforma

| Plataforma | Detectar cambios | Leer/escribir |
|---|---|---|
| Linux Wayland | `wl-clipboard-rs::watch` (`ext-data-control` o `wlr-data-control`; Hyprland soporta ambos) | `wl-clipboard-rs` |
| Linux X11 | XFixes (vía `clipboard-master` o `x11-clipboard`) | `arboard` |
| macOS | Polling de `NSPasteboard.changeCount` (no existe API de notificación) | `arboard` / `objc2` |
| Windows | `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` | `arboard` / crate `windows` |

Todo detrás de un trait `Clipboard` en el crate `clipboard`, con una implementación falsa en memoria
para tests. X11 tiene prioridad baja (el objetivo es Hyprland).

Contrato del trait (detalle completo en los doc comments de `crates/clipboard`):
- `read_text` / `write_text` son síncronos y pueden bloquear un poco; el daemon los llama con
  `spawn_blocking`. Portapapeles vacío o sin texto es `Ok(None)`, no un error.
- `watch` devuelve un **canal** (no un callback): el backend empuja eventos desde su propio hilo del SO
  (Wayland, Windows) o desde un hilo que hace polling (macOS), con un `send` no bloqueante. El canal es
  sin límite porque los eventos son chicos y a velocidad humana.
- Un evento no trae el contenido, solo los **formatos ofrecidos** con su nombre nativo (MIME en Linux,
  UTI en macOS, nombre de formato en Windows), para filtrar sensibles antes de leer.
- Se reportan **todos** los cambios, incluidos los que causa el propio daemon al escribir; el anti-eco
  se hace en el daemon por hash, igual en todas las plataformas.
- `ContentHash` es BLAKE3 de los bytes del contenido (texto: UTF-8 tal cual, sin normalizar).

### Contenido sensible

Se detectan y respetan estos marcadores en **todas** las plataformas:

- **Windows:** formato `ExcludeClipboardContentFromMonitorProcessing` (presente = no sincronizar),
  `CanUploadToCloudClipboard` = 0 y `CanIncludeInClipboardHistory` = 0.
- **Linux:** MIME `x-kde-passwordManagerHint` (valor `secret`). Lo usa KeePassXC, entre otros.
- **macOS:** `org.nspasteboard.ConcealedType`, `org.nspasteboard.TransientType`,
  `org.nspasteboard.AutoGeneratedType` (estándar de nspasteboard.org; 1Password lo usa) y el legado
  `com.agilebits.onepassword`.

### macOS: privacidad del pasteboard (riesgo principal)

Desde macOS 15.4, Apple tiene preparado un aviso cuando una app **lee** el pasteboard de forma
programática. A la fecha no está activo por defecto, pero un daemon que lee en cada cambio es justo el caso
que dispara el aviso cuando se active. Mitigaciones previstas:
- Consultar `NSPasteboard.accessBehavior` al arrancar y reportar el estado en `clipbroad status`.
- Usar los métodos `detect` para saber qué tipos hay **sin leer** antes de decidir leer.
- Documentar el permiso en Ajustes del Sistema → Privacidad y seguridad → "Pegar desde otras apps"
  (Allow). No existe un entitlement que lo evite.
- Por verificar en el spike de la fase 1: si leer solo `changeCount` dispara el aviso.

## Imágenes

Se normalizan a PNG en la red (macOS usa TIFF/PNG; Windows usa DIB/PNG). Van inline mientras estén bajo
el umbral de tamaño.

## Archivos

El portapapeles del SO guarda **referencias** a archivos (URIs, `CF_HDROP`, URLs de archivo), no su
contenido. Sincronizar archivos significa transferir el contenido, guardarlo en una caché local
(`<cache>/clipbroad/files/<hash>/<nombre>`) y poner en el portapapeles las rutas locales.

- Primera versión: **transferencia inmediata** al copiar, con un límite de tamaño configurable. Por
  encima del límite, el archivo no se sincroniza.
- La transferencia perezosa (traer el archivo solo al pegar) existe en cada SO (Wayland es perezoso por
  diseño; macOS tiene `NSFilePromise`; Windows tiene delayed rendering), pero es compleja. Se pospone.
- Transporte: `iroh-blobs` (BLAKE3, streaming verificado, reanudable) encaja muy bien, **pero** su línea
  compatible con iroh 1.x se declara "not yet production quality". Se decide en la fase 4 entre
  iroh-blobs y un ALPN propio de transferencia simple.

## Ciclo de vida del daemon

- Linux: servicio de usuario de systemd.
- macOS: LaunchAgent.
- Windows: tarea al iniciar sesión.
- Configuración en TOML en el directorio de configuración de cada SO; identidad (`SecretKey`) en un
  archivo aparte con permisos restringidos.

## Fases

| Fase | Contenido | Lista cuando… |
|---|---|---|
| 0 | Workspace, CI en 3 SO, AGENTS.md, este documento | El CI está verde en los 3 SO |
| 1 | Backends de portapapeles (texto) + `clipbroad watch` que imprime los cambios + spike de privacidad en macOS | Imprime lo copiado en cada SO; el comportamiento en macOS está documentado aquí |
| 2 | Red P2P, emparejamiento, sincronización de texto | Copiar en una máquina y pegar en las otras dos |
| 3 | Anti-eco, filtro de sensibles, servicio de arranque, `clipbroad status` | Un día de uso real sin problemas |
| 4 | Imágenes, luego archivos | Captura de pantalla en una máquina, pegar en otra; archivo de 100 MB |
| 5 | Icono en bandeja, historial (opcional) | — |

## Estrategia de pruebas

- La lógica (protocolo, anti-eco, filtros) se prueba con el portapapeles falso en memoria.
- Tests de integración que levantan 2–3 nodos en el mismo proceso y verifican la propagación.
- El CI corre en Linux, macOS y Windows; es la única forma de validar macOS/Windows desde la devbox.
- Lo que depende del SO real (eventos del portapapeles, permisos de macOS) se valida con una lista de
  pruebas manuales por plataforma, que se mantiene en `docs/` a partir de la fase 1.
