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
- **Descubrimiento en LAN** con mDNS (ver "Descubrimiento en la LAN (mDNS)" más abajo).
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

### Descubrimiento en la LAN (mDNS)

Cada nodo anuncia por mDNS su `EndpointId` y sus direcciones, y escucha los anuncios de los demás.
Así un peer emparejado se encuentra aunque su IP en la LAN haya cambiado (DHCP), sin tocar `addrs`.
Las direcciones estáticas siguen sirviendo para lo que mDNS no alcanza, como Tailscale.

- **Crate: `iroh-mdns-address-lookup`** (de n0, los autores de iroh). Se descartó
  `iroh-mdns-peer-lookup`: su licencia dice "TBD" (no pasa `cargo deny`) y pide iroh con las features
  por defecto, que traen relays y `portmapper`. El elegido usa la misma versión de iroh que el resto
  (no hay dos iroh en el `Cargo.lock`), es MIT o Apache-2.0 y no agrega avisos ni licencias nuevas.
- **Integración:** se registra como *address lookup* del endpoint (el preset `Minimal` no trae
  ninguno). Al marcar a un peer, iroh le pide a mDNS las direcciones actuales del peer y las
  prueba con las estáticas. Por eso, con mDNS activo, el nodo marca también a los peers sin `addrs`.
- **Nombre de servicio propio:** `clipbroad` (registros `<id>._clipbroad._udp.local`) en vez del
  `irohv1` por defecto, para no mezclarse con otras apps de iroh en la misma red. Todos los nodos
  deben usar el mismo.
- **Solo encuentra direcciones, nunca da acceso.** La allowlist no cambia: un nodo que se anuncia en
  la LAN y no está en la config se rechaza igual después del TLS.
- **Privacidad:** el anuncio es multicast en la red local, así que cualquiera en la misma LAN ve el id
  público de este nodo, sus direcciones y que corre clipbroad. El id no es secreto (no sirve para
  conectarse sin estar emparejado), pero permite reconocer a la máquina en esa red. No se usa ningún
  servicio de terceros: el tráfico no sale de la LAN.
- **Config:** `mdns = true` por defecto. Con `mdns = false` no se abre ningún socket multicast ni se
  envía nada, y solo se marca a los peers con `addrs`.
- **Si no puede arrancar** (por ejemplo, ninguna interfaz permite multicast), el daemon deja un
  warning y sigue solo con las direcciones estáticas; no vuelve a intentarlo hasta reiniciarse.
- **Tests:** el test de dos nodos que se encuentran sin direcciones necesita multicast entre sockets
  locales, que los runners de CI pueden no permitir, así que está marcado `#[ignore]` y se corre a mano
  (`cargo nextest run -p clipbroad-net --run-ignored only`).

### Conexiones (crate `net`)

- **Una conexión por peer**, de larga vida. Cada nodo marca a todos los peers que puede encontrar
  (con mDNS activo, todos; si no, los que tienen direcciones en la config) y acepta las conexiones que
  le llegan de ellos. Sin mDNS, un peer sin direcciones solo puede conectarse marcando él.
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
  pierden. `Node::links()` es un canal `watch` con el estado de cada peer (conectado o no, y la
  dirección del camino que usa la conexión más nueva al conectarse), para `clipbroad status`.

### Emparejamiento

El usuario corre `clipbroad pair` en una máquina (A), que muestra un ticket y espera, y
`clipbroad pair <ticket>` en la otra (B). Ambas agregan a la otra en su configuración y terminan.
Código en `crates/net/src/pair.rs` y `crates/daemon/src/pair.rs`.

- **Ticket:** una línea en base32 con un byte de versión, el `EndpointId` de A, sus direcciones (en
  su puerto de escucha) y un secreto aleatorio de 16 bytes.
- **Flujo:** B marca a A con un ALPN aparte (`clipbroad-pair/1`), fuera de la allowlist, y le manda el
  secreto, su hostname y sus direcciones. A compara el secreto en tiempo constante; si coincide, guarda
  a B y recién entonces le responde con su hostname, y B guarda a A. Así B solo guarda a A si A ya
  guardó a B.
- **Puerto:** el emparejamiento escucha en el `listen_port` normal en ambas máquinas, así las
  direcciones que se intercambian (y la dirección desde la que B se conecta, que A también guarda) son
  las que usará `clipbroad run`. Por eso hay que detener `clipbroad run` mientras se empareja; si está
  corriendo, `pair` falla con un mensaje que lo dice. No cuesta nada: el daemon lee la config solo al
  arrancar, así que igual hay que reiniciarlo para que vea al peer nuevo.
- **Config:** se agrega un bloque `[[peers]]` como texto al final del archivo (se conservan los
  comentarios), con `id`, `name` y `addrs`. Se crea el archivo si no existe, se valida el resultado
  antes de escribir, y un peer que ya estaba no se duplica ni se modifica.
- **Amenazas:** el TLS de iroh ya autentica los dos `EndpointId`; el secreto solo prueba que B vio el
  ticket en la pantalla de A. A acepta un único pedido válido y deja de escuchar, o deja de escuchar a
  los 5 minutos: un ticket filtrado después de usado o vencido no sirve, porque ya nadie responde. Un
  secreto incorrecto solo cierra esa conexión y la sesión sigue esperando (adivinar 128 bits en 5
  minutos no es factible), así un intento ajeno no le arruina el emparejamiento al usuario. Si alguien
  usa un ticket filtrado *antes* que B, queda emparejado con A; el usuario lo nota porque su
  `clipbroad pair <ticket>` falla, y debe quitar ese peer de la config.
- **Límite:** las direcciones guardadas son las del momento. Si cambia la IP de una máquina en la LAN,
  mDNS encuentra la nueva; las guardadas solo quedan desactualizadas. Con `mdns = false` hay que editar
  `addrs` a mano (o emparejar de nuevo después de borrar el peer).

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
  emisor no codifica un frame más grande. Por eso la config rechaza un `inline_max_bytes` mayor que
  16 MiB menos 64 KiB (margen para los metadatos del clip), y el daemon no sincroniza un texto copiado
  más grande que `inline_max_bytes`: lo salta con un warning.
- **Mensajes:** `Hello { protocol_version }` y `Clip`. Cada lado manda `Hello` primero; `decode` devuelve
  un error tipado si la versión no coincide, y la conexión se cierra.
- **`Hello` es estable entre versiones:** siempre es la variante 0 y `protocol_version` siempre es su
  primer campo; solo se le pueden agregar campos al final. `decode` ignora los bytes sobrantes dentro de un
  frame, así un nodo viejo puede leer la versión de un `Hello` nuevo y rechazarlo con un error claro.
- **`Clip`:** `id` (número que elige el origen), `origin` (los 32 bytes del `EndpointId`, opacos para
  `core`), `timestamp_ms` (milisegundos Unix según el reloj del origen, adelantado si hace falta para quedar
  después de lo último que vio; ver "Semántica de sincronización"), `kind`, `mime`, `size`, `hash` y
  `payload`. El payload hoy solo puede ser inline; el anuncio de archivos está bosquejado en
  "Transporte de archivos" y se agrega en la fase 4.
- **Verificación al recibir:** el contenido solo se obtiene con `Clip::into_content`, que comprueba que el
  tamaño y el hash BLAKE3 declarados coinciden con el payload, que el texto sea UTF-8 válido y que una
  imagen empiece como PNG.
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

### Daemon (`clipbroad run`)

Un solo bucle une las piezas (`crates/daemon/src/run.rs`), con el mismo flujo que describe el módulo
`sync`: un cambio local con formatos se lee (fuera del hilo async), y si es texto o imagen nuevo se
envía a todos (ver "Imágenes" para cuál gana si hay ambos); un clip recibido se escribe en el
portapapeles si el motor responde `Apply`. Un error al leer, escribir o procesar un clip se registra y
el daemon sigue; solo una falla al arrancar o del watcher del portapapeles lo detiene. Se apaga con
Ctrl+C o SIGTERM.

- Los logs muestran el id del nodo al arrancar, las conexiones y cada clip enviado o recibido con su
  tamaño y los primeros 8 caracteres del hash, nunca el contenido. `run` registra en nivel info por
  defecto.
- `--config <archivo>` (en `run` y `pair`) usa otro archivo de config, y `secret.key` en la misma
  carpeta como identidad. Sirve para correr dos instancias en una máquina (pruebas).
- El contenido marcado como sensible no se envía: el daemon lo salta con un log en nivel debug que
  muestra el marcador, nunca el contenido.

### Estado del daemon (`clipbroad status`)

El daemon escribe su estado en un archivo TOML, `status.toml`, en la misma carpeta que `secret.key`
(código en `crates/daemon/src/status.rs`). `clipbroad status` lo lee y lo muestra.

- **Archivo y no socket o named pipe:** el mismo código sirve en los tres SO, el daemon no tiene que
  atender a nadie y un daemon caído se nota porque el archivo deja de actualizarse. Un socket daría
  el estado al instante, pero pide un servidor dentro del daemon y código distinto en Windows.
- **Cuándo se escribe:** después de cada evento (un peer conecta o se va, un clip enviado o
  recibido) y cada 5 s. Se escribe a un archivo temporal y se renombra, así quien lee nunca ve un
  archivo a medias. Al detenerse, el daemon lo borra. Recién se escribe con el nodo ya levantado:
  un segundo daemon con la misma config falla antes (el puerto está ocupado) y no pisa el archivo.
- **Vivo o no:** sin archivo, el daemon no corre. Si el archivo tiene más de 15 s, el daemon se
  cortó sin borrarlo (`kill -9`, un crash, un corte de luz) y `status` lo muestra como detenido. No
  se revisa el PID: haría falta código por SO y la antigüedad ya alcanza.
- **Ubicación:** junto a la identidad, así dos instancias con su propio `--config` (y por lo tanto
  su propia `secret.key`) nunca comparten el archivo. `status --config <archivo>` lee el de esa
  instancia.
- **Qué muestra:** si el daemon corre (y su PID), su `EndpointId`, el puerto, si mDNS está activo,
  cada peer con su nombre, id corto y si está conectado (y por qué dirección), y el último clip
  enviado y recibido con hora, tamaño y hash corto. Nunca el contenido. En macOS 15.4 o más nuevo,
  también el `accessBehavior` del pasteboard tal como lo ve el daemon, con instrucciones si no está
  en "Allow".

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
- `read_image` / `write_image` siguen las mismas reglas con imágenes PNG (ver "Imágenes"). Son
  opcionales: por defecto `read_image` devuelve `Ok(None)` y `write_image` devuelve
  `ClipboardError::Unsupported`, así un backend sin imágenes (hoy macOS y Windows) compila igual.
- Ninguna lectura o escritura bloquea más de 2 segundos: si la app dueña del contenido no responde, el
  backend devuelve error. El límite lo pone el backend porque el daemon no puede cancelar un hilo bloqueado.
- `watch` devuelve un **canal** (no un callback): el backend empuja eventos desde su propio hilo del SO
  (Wayland, Windows) o desde un hilo que hace polling (macOS), con un `send` no bloqueante. El canal es
  sin límite porque los eventos son chicos y a velocidad humana.
- Un evento no trae el contenido, solo los **formatos ofrecidos** con su nombre nativo (MIME en Linux,
  UTI en macOS, nombre de formato en Windows).
- `read_text` y `read_image` nunca leen contenido sensible: revisan los marcadores sobre el mismo
  contenido que leen y devuelven `ClipboardError::Sensitive` sin leer los datos (ver "Contenido
  sensible").
- Se reportan **todos** los cambios, incluidos los que causa el propio daemon al escribir; el anti-eco
  se hace en el daemon por hash, igual en todas las plataformas.
- `ContentHash` es BLAKE3 de los bytes del contenido (texto: UTF-8 tal cual, sin normalizar; imagen:
  los bytes PNG).

### Contenido sensible

Se detectan y respetan estos marcadores en **todas** las plataformas:

- **Windows:** formato `ExcludeClipboardContentFromMonitorProcessing` (presente = no sincronizar),
  `CanUploadToCloudClipboard` = 0 y `CanIncludeInClipboardHistory` = 0.
- **Linux:** MIME `x-kde-passwordManagerHint` (valor `secret`). Lo usa KeePassXC, entre otros.
- **macOS:** `org.nspasteboard.ConcealedType`, `org.nspasteboard.TransientType`,
  `org.nspasteboard.AutoGeneratedType` (estándar de nspasteboard.org; 1Password lo usa) y el legado
  `com.agilebits.onepassword`.

Cómo se aplica (código en `crates/core/src/sensitive.rs` y en cada backend):

- **Una sola lista en `core`.** `sensitive_marker(formatos)` compara nombres exactos (mayúsculas
  incluidas). Los dos flags de Windows guardan un DWORD y solo marcan si valen 0, así que el backend
  de Windows lee el valor y los pasa como `CanUploadToCloudClipboard=0`.
- **Linux por presencia.** `x-kde-passwordManagerHint` marca aunque no se lea su valor: KeePassXC
  pone `secret` y ninguna app lo ofrece para decir "no es secreto". Leer el valor sería una
  transferencia más desde la app dueña.
- **El filtro está en `read_text`, no en el evento.** Entre el evento y la lectura puede llegar otra
  copia: si se revisaran los formatos del evento, un secreto copiado justo después se leería con los
  formatos del contenido anterior. Por eso cada backend lista los formatos y lee el texto del mismo
  contenido, y si encuentra un marcador devuelve `ClipboardError::Sensitive` sin leer el texto. Que
  sea un error hace que quien lo olvide manejar igual se quede sin el texto. `read_image` hace lo
  mismo con la imagen.
  - **Wayland:** pide al compositor la oferta actual, que trae su lista de MIME fija, y lee el texto
    de esa misma oferta. Una copia nueva es otra oferta, así que la lista y el texto siempre son del
    mismo contenido.
  - **Windows:** con el portapapeles abierto (`OpenClipboard`) nadie más puede cambiarlo. Lista los
    formatos, lee los DWORD de los flags y el texto antes de cerrarlo.
  - **macOS:** no hay forma de hacerlo en un paso. Lee `changeCount`, revisa los tipos, lee el texto,
    revisa los tipos otra vez y compara `changeCount`. Si cambió, empieza de nuevo (hasta 3 veces) y
    después devuelve error.
- **Falla cerrado.** Si no se pueden listar los formatos o leer el valor de un flag, `read_text`
  devuelve error y el daemon no envía nada.
- `clipbroad watch` muestra los formatos del evento y, si el contenido es sensible, el marcador en
  vez del texto. Sirve para comprobar qué marca cada gestor de contraseñas.

Riesgos que quedan:

- **Apps que no marcan.** Lo que un gestor de contraseñas copie sin ningún marcador (o copiar una
  contraseña a mano desde otra app) se sincroniza. Pasa también con un gestor que use un marcador
  que no esté en la lista.
- **macOS, marcador tardío.** Escribir tipos uno por uno no cambia `changeCount`. Si una app escribe
  el texto y agrega el marcador después de que el daemon revisó los tipos por segunda vez, el texto
  se envía. La segunda revisión acorta esa ventana a lo que tarda leer el texto. Las apps que siguen
  nspasteboard.org declaran todos los tipos antes de escribir datos, y no tienen este problema.
- **Windows, eventos sin valor.** El evento muestra `CanUploadToCloudClipboard` sin su valor: leerlo
  en el hilo del watcher podría bloquearlo esperando a la app que copió. El valor solo se lee en
  `read_text`, que tiene límite de tiempo, y `watch` lo muestra en el marcador (`=0`).
- **Comportamiento real en macOS y Windows** con 1Password, KeePassXC y Bitwarden: solo se prueba a
  mano (los tests de CI escriben los marcadores ellos mismos).

### macOS: privacidad del pasteboard (riesgo principal)

Desde macOS 15.4, Apple tiene preparado un aviso cuando una app **lee** el pasteboard de forma
programática. A la fecha no está activo por defecto, pero un daemon que lee en cada cambio es justo el caso
que dispara el aviso cuando se active. Mitigaciones previstas:
- Consultar `NSPasteboard.accessBehavior` y reportar el estado en `clipbroad status`. Lo consulta el
  daemon cada vez que actualiza su estado, porque el permiso es del proceso que lee.
- Usar los métodos `detect` para saber qué tipos hay **sin leer** antes de decidir leer.
- Documentar el permiso en Ajustes del Sistema → Privacidad y seguridad → "Pegar desde otras apps"
  (Allow). No existe un entitlement que lo evite.
- Por verificar en el spike de la fase 1: si leer solo `changeCount` dispara el aviso.

## Imágenes

Se normalizan a PNG en la red (macOS usa TIFF/PNG; Windows usa DIB/PNG). Van inline mientras estén bajo
el umbral de tamaño. Código en `crates/core/src/content.rs` (`Png`) y `protocol.rs`.

- **En la red siempre PNG.** `ClipContent::Image(Png)` viaja como `ContentKind::Image` con MIME
  `image/png`. El hash es BLAKE3 de los bytes PNG tal como viajan. Un texto y una imagen nunca tienen
  los mismos bytes: un PNG empieza con `0x89`, y ningún texto UTF-8 puede empezar así.
- **La conversión vive en los backends, no en `core`.** Cada backend convierte su formato nativo a PNG
  al leer y PNG a su formato nativo al escribir (TIFF en macOS, DIB en Windows; Wayland ya ofrece
  `image/png`). Así `core` no depende del crate `image`, que es pesado; solo lo trae el backend que lo
  necesite, con las features mínimas (tarea de backends de imágenes).
- **`core` solo revisa el encabezado.** `Png::try_from` acepta bytes que empiezan con la firma PNG
  seguida del chunk `IHDR`, y rechaza el resto con `NotPng`. No decodifica la imagen: un PNG roto más
  allá del encabezado falla en la app que pega, igual que si se hubiera copiado en la misma máquina.
  `Clip::into_content` rechaza un clip de imagen que no empieza como PNG (`ClipError::NotPng`), así un
  peer no puede poner bytes cualquiera en el portapapeles como imagen.
- **`PROTOCOL_VERSION` pasa a 2.** Un nodo con la versión 1 no puede decodificar `ContentKind::Image`:
  el frame le resulta malformado y cierra la conexión con cada imagen copiada. Con la versión nueva, dos
  nodos de versiones distintas se rechazan al conectar con el error claro de `Hello`, y hay que
  actualizar todas las máquinas. El ALPN sigue siendo `clipbroad/1`: si cambiara, la conexión fallaría
  en el handshake sin decir por qué.
- **Una imagen más grande que `inline_max_bytes` no se sincroniza:** el daemon la salta con un warning,
  igual que al texto. Cuando exista el canal de archivos (ver "Archivos") se puede reconsiderar enviarla
  por ahí.
- **Requisito para los backends (anti-eco):** el anti-eco compara hashes. Si un backend escribe un PNG y
  al leerlo de vuelta entrega otros bytes (por ejemplo, porque lo re-codifica desde TIFF), el daemon lo ve
  como una copia nueva y la reenvía. Dos máquinas que re-codifican podrían reenviarse la imagen sin fin.
  Por eso un backend debe devolver los mismos bytes PNG que escribió, por ejemplo escribiendo el tipo PNG
  y leyéndolo primero cuando está.
- **Texto e imagen en la misma copia: gana el texto.** El daemon llama a `read_text` y, solo si no hay
  texto, a `read_image` (`read_content` en `crates/daemon/src/run.rs`). Las apps que ofrecen ambos en
  una copia (celdas de una planilla, un documento de oficina) lo hacen sobre todo para que el texto se
  pueda pegar también como dibujo; lo que la persona quiso copiar es el texto, que además pesa mucho
  menos. Un clip lleva un solo contenido, así que no se pueden mandar los dos.
  - **Excepción, "Copiar imagen" del navegador:** Chrome ofrece la imagen y `text/html` con una
    etiqueta `<img>`, sin texto plano. Por contrato, si la copia trae una imagen legible, `read_text`
    solo cuenta el texto plano (en Wayland: `text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`,
    `STRING`, `TEXT`), así que ahí gana la imagen y no se sincroniza el HTML.
  - **Límite conocido:** Firefox, al copiar una imagen, ofrece además la URL como texto plano. Ahí
    gana la URL. Arreglarlo pediría mandar varios formatos en un clip.
- **Wayland:** lee y escribe solo `image/png`, byte por byte, desde la misma oferta cuya lista de MIME
  revisó (igual que el texto). Una copia que solo ofrece otros tipos (`image/jpeg`, `image/bmp`)
  cuenta como sin imagen: convertirla pediría un códec. Las herramientas de captura (`grim`) y los
  navegadores ofrecen `image/png`.
- **macOS y Windows todavía no soportan imágenes:** no las leen, y el daemon que recibe una la salta
  con un warning ("does not support images yet") sin tocar el portapapeles. Llegan en PRs aparte.
- `clipbroad watch` muestra una imagen como `(image/png, 12345 bytes)` y `clipbroad status` muestra el
  tipo del último clip (`text` o `image`) junto al tamaño y el hash.

## Archivos

El portapapeles del SO guarda **referencias** a archivos (URIs, `CF_HDROP`, URLs de archivo), no su
contenido. Sincronizar archivos significa transferir el contenido, guardarlo en una caché local
(`<cache>/clipbroad/files/<hash>/<nombre>`) y poner en el portapapeles las rutas locales.

- Primera versión: **transferencia inmediata** al copiar, con un límite de tamaño configurable. Por
  encima del límite, el archivo no se sincroniza.
- La transferencia perezosa (traer el archivo solo al pegar) existe en cada SO (Wayland es perezoso por
  diseño; macOS tiene `NSFilePromise`; Windows tiene delayed rendering), pero es compleja. Se pospone.
### Transporte de archivos

**Decisión: transferencia propia, sin `iroh-blobs`.** Cada archivo viaja en un stream bidireccional de
QUIC dentro de la conexión `clipbroad/1` que ya existe con el peer. El que recibe lo pide por hash y lo
escribe en un archivo temporal mientras calcula el BLAKE3. Solo lo mueve a la caché si el tamaño y el
hash coinciden.

**Medición** (spike 4.3, prototipo descartable). Dos procesos en la misma máquina por localhost, build
release, disco btrfs, 3 corridas. La memoria es el RSS máximo del proceso entero, iroh incluido.

| | 100 MB | 1 GB | RSS emisor | RSS receptor | Disco del receptor |
|---|---|---|---|---|---|
| Propio | 140–155 ms (650–715 MB/s) | 1,37 s | 17 MiB | 18 MiB (20 con 1 GB) | el archivo |
| `iroh-blobs` con `fs-store` | 147–155 ms con la exportación (645–680 MB/s) | 1,33 s | 20 MiB | 21–22 MiB (23 con 1 GB) | el archivo en su store y la copia exportada |

Las dos opciones tienen la misma velocidad y su memoria no crece con el archivo. Los datos no
favorecen a ninguna, así que deciden la complejidad y el riesgo. Preparar el envío tarda 17 ms por
100 MB con la propia (solo BLAKE3) y 54 ms con iroh-blobs (importar al store calcula además el árbol
de verificación). Localhost no mide la red: en una LAN real el límite es el enlace (unos 110 MB/s en
gigabit, menos en Wi-Fi). No se midió en macOS ni en Windows.

Por qué no `iroh-blobs` (0.103.1, la única línea compatible con iroh 1.x):
- **No está lista para producción, según su README.** Recomienda la 0.35 para producción, pero esa
  versión exige iroh 0.35 y no convive con nuestro iroh 1.4.
- **Bug de seguridad recién corregido.** Hasta la 0.103.0, con la configuración por defecto cualquier
  peer conectado podía escribir blobs en el store del otro (pedidos *push* sin permiso,
  n0-computer/iroh-blobs#270). Se corrigió en la 0.103.1 (octubre de 2026) y se retiraron de crates.io
  las versiones 0.97 a 0.103.0.
- **Bugs abiertos en el store en disco** que nos afectarían: el GC borra blobs que siguen en uso (#247,
  #256, #314), una falla del GC lo detiene para siempre (#291), corrupción de la base `redb` con I/O
  pesado (#261), RSS que crece al reimportar (#266). Un SIGKILL a mitad de descarga pierde el progreso
  (#254), así que la reanudación no sobrevive a un crash.
- **API inestable.** Publicó 9 versiones menores entre octubre de 2025 y junio de 2026, y en 0.x cada
  una puede romper la API. No promete estabilidad del formato del store.
- **Peso.** Con las features mínimas agrega 29 crates, pero así solo trae el store en memoria, que
  carga el archivo entero en RAM. Con `fs-store` agrega 31 (suma `redb` y `reflink-copy`). Las licencias
  pasan, pero `cargo deny` falla por RUSTSEC-2024-0370 (`proc-macro-error` sin mantenimiento, vía
  `genawaiter`) y habría que ignorar otro aviso. También suma `syn` 1.x y otra versión de `spin`.
- **Encaje.** El store es una base de datos aparte, con tags y GC que administrar además de la caché
  `<cache>/clipbroad/files/<hash>/<nombre>`. El receptor guarda en el store y después exporta a la
  caché: en btrfs o APFS es un clon, pero en ext4 o NTFS escribe el archivo dos veces. El proveedor le
  entrega cualquier blob del store a cualquier peer conectado, y hay que configurar `EventMask` para
  rechazar los *push*.
- **Lo que aporta no hace falta aquí:** verificación por rangos, reanudación, descarga desde varios
  peers y colecciones. En la LAN un archivo de 100 MB tarda alrededor de un segundo, y reintentar desde
  cero es aceptable.

Por qué la conexión existente y no un ALPN aparte (`clipbroad-file/1`):
- La conexión ya está autenticada y pasó la allowlist. Con un ALPN aparte, el receptor tendría que
  marcar al emisor, y sin mDNS un peer sin direcciones solo se conecta si el otro lo marca (ver
  "Conexiones"). En ese caso el receptor no podría pedir el archivo.
- QUIC multiplexa los streams, así que un archivo grande no frena los frames de clips. El stream de
  clips tiene más prioridad (`SendStream::set_priority`) para que un texto copiado no espere detrás de
  un archivo.
- Los streams se distinguen por tipo. Cada lado sigue abriendo un solo stream unidireccional para los
  clips, y cada stream bidireccional es un pedido de archivo.
- Costo: si la conexión se cae o se reemplaza (ver "Conexiones duplicadas"), la transferencia en curso
  se corta y se reintenta en la conexión nueva.

Transferencia (borrador para la tarea de archivos):
- **Pedido.** El receptor abre un stream bidireccional y manda un frame `FileRequest { hash }`, con el
  mismo enmarcado que los demás mensajes. El emisor responde con un frame `FileResponse::Ok { size }`
  seguido de los bytes crudos del archivo hasta cerrar el stream, o con `FileResponse::NotOffered`.
- **El emisor solo sirve los archivos de su último clip anunciado** (una tabla hash → ruta). Los lee de
  su ruta original, sin copiarlos. Un hash que no está en la tabla recibe `NotOffered`.
- **El receptor verifica antes de exponer.** Revisa que `size` sea el anunciado y no pase el límite.
  Escribe en un temporal dentro de `<cache>/clipbroad/files/<hash>/`, actualiza el BLAKE3 con cada
  bloque y corta si llegan más bytes que `size`. Al terminar compara tamaño y hash: si coinciden,
  renombra el temporal al nombre final, y si no, lo borra. El portapapeles solo recibe rutas de
  archivos verificados.
- Si el archivo cambió en el emisor después de copiarlo, el hash no coincide y el clip no se aplica.
- Un archivo que ya está en la caché con ese hash no se pide.
- Un clip más nuevo cancela las transferencias del anterior (cierra sus streams).
- Sin reanudación: si algo falla, se borra el temporal y se pide de nuevo desde cero.

Anuncio en `Clip` (borrador, sin implementar):

```rust
pub enum ContentKind { Text, Image, Files }

pub enum Payload {
    Inline(Vec<u8>),
    Files(Vec<FileEntry>),
}

pub struct FileEntry {
    pub name: String,       // solo el nombre del archivo, nunca una ruta
    pub size: u64,
    pub hash: ContentHash,  // BLAKE3 del contenido
}
```

- `kind` es `Files`, `mime` es `text/uri-list`, `size` es la suma de los tamaños y `hash` es el BLAKE3
  de la lista codificada (nombre, tamaño y hash de cada archivo, en orden). Ese hash no depende de la
  ruta del archivo en cada máquina, así que el anti-eco funciona como con el texto. Al escribir las
  rutas de la caché en el portapapeles, el evento local calcula el mismo hash desde los archivos, lo
  que cuesta unos 17 ms por cada 100 MB.
- `Clip::into_content` revisa que la lista no esté vacía, que cada nombre sea un solo componente
  válido (sin `/`, `\`, `..`, NUL ni nombres reservados de Windows), que no se repitan nombres, que la
  suma de tamaños sea `size`, que el hash de la lista sea `hash` y que el total no pase el límite.
- Varios archivos por copia entran en la misma lista. Cada backend la traduce a su formato:
  `text/uri-list` en Wayland, `CF_HDROP` en Windows, URLs de archivo en macOS.
- Las carpetas quedan fuera de la primera versión: se saltan con un warning.
- El límite es un total por clip en la config. El emisor no anuncia un clip que lo pasa, y el receptor
  aplica su propio límite antes de pedir y mientras recibe.
- `PROTOCOL_VERSION` pasa a 3.

Riesgos:
- **Código propio que mantener.** El prototipo tiene unas 70 líneas entre los dos lados. Con tests y el
  manejo de errores, se estiman unas 300 en `net`.
- **Sin reanudación.** En una red lenta, un archivo grande que se corta empieza de cero. Si pasa en el
  uso real, se puede pedir desde un offset (el temporal ya tiene el comienzo) sin cambiar el anuncio.
- **La verificación es al final.** Un peer emparejado que manda basura nos hace escribir hasta `size`
  bytes antes de detectarla. Es un peer de confianza y el límite acota el daño.
- Se puede volver a evaluar `iroh-blobs` cuando su línea compatible con iroh 1.x se declare lista para
  producción.

## Ciclo de vida del daemon

- `clipbroad service install|uninstall|status` instala el arranque al iniciar sesión con el mecanismo
  de cada SO. El servicio corre la ruta absoluta del ejecutable actual con `run` (y `--config` solo
  si se pasó a `install`), así que mover el binario requiere volver a instalar. `install` es
  idempotente: reescribe la definición y reinicia el daemon.
- Linux: unidad de usuario de systemd en `~/.config/systemd/user/clipbroad.service`, ligada a
  `graphical-session.target` (`WantedBy`, `After`, `PartOf`) y con `Restart=on-failure` cada 5 s. Los
  logs van a journald. Necesita que la sesión importe `WAYLAND_DISPLAY` al gestor de usuario de
  systemd y active `graphical-session.target` (uwsm lo hace); `install` avisa si falta alguno.
- macOS: LaunchAgent en `~/Library/LaunchAgents/` (dominio `gui/<uid>`), con `KeepAlive` solo ante
  fallos. Los logs van a `~/Library/Logs/clipbroad.log`, que crece sin rotación.
- Windows: valor `clipbroad` en `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` en vez de una
  tarea programada: no pide permisos de administrador (`schtasks /sc onlogon` sí) y no hereda los
  valores por defecto de las tareas (se detienen con batería y a los 3 días). A cambio, nada lo
  reinicia si se cae. Se lanza con `conhost.exe --headless` para que no abra una ventana de consola;
  por eso sus logs no quedan en ningún lado. `install` y `uninstall` detienen cualquier otro
  `clipbroad.exe` que esté corriendo.
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
