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
- **Features de iroh**: `tls-ring` y `fast-apple-datapath`, sin las de por defecto. Sin `portmapper`
  (UPnP no hace falta en LAN/Tailscale y trae una dependencia MPL-2.0) ni `metrics`. `cargo deny` solo
  revisa los targets que se publican (así no cuentan los crates solo-wasm de iroh) y permite
  CDLA-Permissive-2.0 únicamente para `webpki-roots`. El aviso RUSTSEC-2024-0436 (`paste` sin
  mantenimiento, no es una vulnerabilidad) se ignora porque iroh lo necesita vía `netwatch`.

### Conexiones (crate `net`)

- **Una conexión por peer**, de larga vida. Cada nodo marca a todos los peers de los que tiene
  direcciones y acepta las conexiones que le llegan de ellos. Un peer sin direcciones en la config
  solo puede conectarse marcando él.
- **Streams:** cada lado abre un stream unidireccional y escribe frames: primero `Hello`, luego un
  frame por clip. Lee el stream del otro lado igual. No hay pedido/respuesta.
- **Conexiones duplicadas:** si los dos marcan a la vez quedan dos conexiones. Decide solo el nodo con
  el id menor: se queda con la que terminó el intercambio de `Hello` más recientemente y cierra la
  otra. El nodo con el id mayor no cierra nada. Así ambos convergen sin acordar nada, y "la más
  nueva gana" también cubre a un peer que se reinició mientras su conexión vieja parecía viva.
- **Reconexión:** con backoff exponencial entre 0,5 s y 30 s por peer. Vuelve al mínimo después de una
  sesión que llegó a intercambiar `Hello`; un handshake que falla (por ejemplo, versión distinta)
  sigue subiendo el retraso. El `Hello` tiene un límite de 10 s.
- **Sin reenvío:** la malla es completa, así que un clip debe venir de la máquina que lo copió. Si
  `Clip.origin` no es el peer que lo manda, se descarta.
- **API:** `Node::spawn(identidad, config)` devuelve el nodo y un canal de clips recibidos (con el id
  del peer). `Node::broadcast(clip)` lo manda a los peers conectados; los que no lo están se lo
  pierden.

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

Detalles (código en `crates/core/src/protocol.rs`):
- **Frame:** 4 bytes con la longitud (u32 big-endian) y luego el mensaje en `postcard`. `core` solo
  codifica y decodifica bytes (`encode`, `decode`); `net` hace la lectura y escritura en los streams QUIC.
  `decode` distingue "faltan bytes" de un error.
- **Tamaño máximo de frame: 16 MiB**, constante del protocolo (no configurable, porque ambos lados deben
  coincidir). El receptor lo revisa apenas llegan los 4 bytes de longitud, antes de reservar memoria; el
  emisor no codifica un frame más grande. Por eso `inline_max_bytes` debe quedar por debajo de ese límite.
- **Mensajes:** `Hello { protocol_version }` y `Clip`. Cada lado manda `Hello` primero; `decode` devuelve
  un error tipado si la versión no coincide, y la conexión se cierra.
- **`Hello` es estable entre versiones:** siempre es la variante 0 y `protocol_version` siempre es su
  primer campo; solo se le pueden agregar campos al final. `decode` ignora los bytes sobrantes dentro de un
  frame, así un nodo viejo puede leer la versión de un `Hello` nuevo y rechazarlo con un error claro.
- **`Clip`:** `id` (número que elige el origen), `origin` (los 32 bytes del `EndpointId`, opacos para
  `core`), `timestamp_ms` (milisegundos Unix según el reloj del origen, adelantado si hace falta para quedar
  después de lo último que vio; ver "Semántica de sincronización"), `kind`, `mime`, `size`, `hash` y
  `payload`. El payload hoy solo puede ser inline; el anuncio de archivos grandes se agrega en la fase 4.
- **Verificación al recibir:** el contenido solo se obtiene con `Clip::into_content`, que comprueba que el
  tamaño y el hash BLAKE3 declarados coinciden con el payload (y que el texto sea UTF-8 válido).
- El `Debug` de un `Clip` muestra el largo del payload, nunca su contenido, para no filtrarlo en los logs.

### Semántica de sincronización

La lógica vive en `crates/core/src/sync.rs` (`SyncEngine`), sin IO ni reloj: el daemon le pasa la hora.
Ante un evento local, el daemon lee el texto y llama a `local_changed`; si responde `Send(clip)`, lo envía
a todos los peers. Ante un clip remoto, llama a `remote_clip`; si responde `Apply(contenido)`, lo escribe
en el portapapeles local.

- **Último en escribir gana.** Cada copia local nueva se envía a todos los peers; al recibir, se escribe
  en el portapapeles local si es más nueva que lo último visto. Los clips se ordenan por
  `(timestamp_ms, origen)`: el `NodeId` de origen desempata, así todas las máquinas eligen el mismo.
- **Reloj:** los relojes de las máquinas difieren. Con el timestamp del reloj tal cual, una máquina
  atrasada 10 s perdería contra una copia hecha 3 s antes en otra máquina, incluso en ella misma. Por eso
  una copia local lleva `max(ahora, lo más nuevo visto + 1 ms)`: siempre le gana a todo lo que su máquina
  ya vio, sin importar los relojes. Para una persona que copia en una máquina a la vez, siempre gana su
  última copia.
- **Anti-eco:** al escribir contenido remoto en el portapapeles local, el SO dispara un evento de cambio.
  Se guarda el hash del último contenido enviado o aplicado y se ignora el evento local que coincide.
  Sin esto, dos máquinas se reenvían el mismo contenido para siempre. Se necesita desde la primera
  sincronización (fase 2).
- **Mismo texto copiado dos veces seguidas:** no se reenvía, porque el hash coincide con lo último enviado.
  Los peers ya lo tienen: si alguno hubiera copiado algo después, ese clip habría llegado y cambiado el
  hash guardado. Por eso, si A copia X, B copia Y y A vuelve a copiar X, X se envía de nuevo y gana en
  todas.
- **Estado acotado:** solo el último hash y la última marca `(timestamp_ms, origen)`, sin historial ni
  persistencia.
- **Límites conocidos:**
  - Dos copias en máquinas distintas, tan seguidas que ninguna vio la otra, son concurrentes: gana la de
    marca mayor (la máquina con el reloj adelantado). Todas terminan con el mismo contenido.
  - Un reloj adelantado arrastra las marcas de todos hacia adelante; el orden sigue funcionando, pero
    `timestamp_ms` deja de ser la hora exacta.
  - Un daemon recién arrancado no vio nada. Si el reloj de un peer va adelantado, su primera copia puede
    perder contra el último clip de ese peer, como mucho durante la diferencia de relojes.
  - No hay puesta al día al reconectar: un peer que se perdió un clip queda con el contenido anterior
    hasta la próxima copia, y copiar de nuevo el mismo texto no lo reenvía.
- **Contenido sensible: nunca se sincroniza** (ver la sección siguiente).

## Portapapeles por plataforma

| Plataforma | Detectar cambios | Leer/escribir |
|---|---|---|
| Linux Wayland | `wl-clipboard-rs::watch` (`ext-data-control` o `wlr-data-control`; Hyprland soporta ambos) | `wl-clipboard-rs` |
| Linux X11 | XFixes (vía `clipboard-master` o `x11-clipboard`) | `arboard` |
| macOS | Polling de `NSPasteboard.changeCount` (no existe API de notificación) | `arboard` / `objc2` |
| Windows | `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` en una ventana message-only | `windows-sys` directo (`arboard` no expone la lista de formatos) |

Todo detrás de un trait `Clipboard` en el crate `clipboard`, con una implementación falsa en memoria
para tests. X11 tiene prioridad baja (el objetivo es Hyprland).

Contrato del trait (detalle completo en los doc comments de `crates/clipboard`):
- `read_text` / `write_text` son síncronos y pueden bloquear un poco; el daemon los llama con
  `spawn_blocking`. Portapapeles vacío o sin texto es `Ok(None)`, no un error.
- Ninguna lectura o escritura bloquea más de 2 segundos: si la app dueña del contenido no responde, el
  backend devuelve error. El límite lo pone el backend porque el daemon no puede cancelar un hilo bloqueado.
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
  archivo aparte con permisos restringidos. Ejemplo comentado con todas las opciones y rutas:
  [`config.example.toml`](config.example.toml).
- Crate propio `clipbroad-config`: lo usan el daemon, `net` (peers e identidad) y el emparejamiento,
  que escribe en esta configuración.
- Identidad: los 32 bytes crudos de la clave Ed25519 (`iroh::SecretKey::from_bytes` los convierte).
  Se genera la primera vez. En Unix el archivo se crea con permisos `0600` y se rechaza si otros
  usuarios pueden leerlo; en Windows hereda los permisos de `%LOCALAPPDATA%` (solo el usuario,
  administradores y SYSTEM).
- En Windows se usa `%LOCALAPPDATA%` (no la carpeta *roaming*): la identidad y la lista de peers son de
  una máquina y no deben copiarse a otra con el perfil itinerante.
- Las rutas se calculan con la biblioteca estándar en vez del crate `directories`, porque este depende
  de `option-ext` (MPL-2.0), que la política de licencias de `cargo deny` no permite.

## Distribución

- Cada tag `v*` publica un GitHub Release con los 3 binarios (`.tar.gz` / `.zip`) y `SHA256SUMS`. El
  proceso está en [`RELEASE.md`](RELEASE.md).
- Workflow propio en vez de `cargo-dist`: alcanza con 3 archivos y checksums, y un workflow corto escrito
  a mano es más fácil de mantener que uno generado. Se revisa si hacen falta instaladores o Homebrew.
- La versión vive en `[workspace.package]`; el workflow falla si el tag no coincide con
  `clipbroad --version`.
- macOS: sin firma ni notarización por ahora (requiere cuenta de Apple Developer).

## Fases

| Fase | Contenido | Lista cuando… |
|---|---|---|
| 0 | Workspace, CI en 3 SO, AGENTS.md, este documento | El CI está verde en los 3 SO |
| 1 | Backends de portapapeles (texto) + `clipbroad watch` que imprime los cambios + spike de privacidad en macOS | Imprime lo copiado en cada SO; el comportamiento en macOS está documentado aquí |
| 2 | Red P2P, emparejamiento, sincronización de texto con anti-eco | Copiar en una máquina y pegar en las otras dos |
| 3 | Filtro de sensibles, servicio de arranque, `clipbroad status` | Un día de uso real sin problemas |
| 4 | Imágenes, luego archivos | Captura de pantalla en una máquina, pegar en otra; archivo de 100 MB |
| 5 | Icono en bandeja, historial (opcional) | — |

## Estrategia de pruebas

- La lógica (protocolo, anti-eco, filtros) se prueba con el portapapeles falso en memoria.
- Tests de integración que levantan 2–3 nodos en el mismo proceso y verifican la propagación.
- El CI corre en Linux, macOS y Windows; es la única forma de validar macOS/Windows desde la devbox.
- Lo que depende del SO real (eventos del portapapeles, permisos de macOS) se valida con una lista de
  pruebas manuales por plataforma, que se mantiene en `docs/` a partir de la fase 1.
