# AGENTS.md

Instrucciones para agentes de código que trabajan en este repo. `CLAUDE.md` solo importa este archivo;
editar siempre aquí.

## Proyecto

`clipbroad` sincroniza el portapapeles entre las máquinas propias (Linux/Hyprland, macOS, Windows) con
un daemon P2P en Rust, sin servidor. **Leer `docs/DESIGN.md` antes de cualquier cambio no trivial**: ahí
están las decisiones de arquitectura, el plan por fases y los riesgos conocidos.

El dueño del proyecto supervisa por resultados y no lee Rust en detalle. Por eso:
- Cada cambio debe dejar evidencia verificable: tests nuevos, salida de comandos, o pasos de prueba
  manual explícitos.
- Explicar en el PR **qué** cambió y **cómo se verificó**, en lenguaje simple.
- Si algo no se pudo verificar (por ejemplo, comportamiento real en macOS o Windows), decirlo
  explícitamente.

## Estructura

```
crates/core       # tipos de contenido, protocolo, anti-eco, filtros. Sin código de SO.
crates/clipboard  # trait Clipboard + backends por SO + fake en memoria para tests
crates/net        # transporte iroh, emparejamiento, broadcast
crates/daemon     # binario `clipbroad` (CLI + orquestación)
docs/DESIGN.md    # decisiones y plan
```

Reglas de capas: `core` no depende de nada del SO ni de la red. Todo lo que toca el SO vive en
`clipboard` detrás de `#[cfg(target_os = ...)]`. `daemon` solo une piezas.

## Comandos

Las herramientas se instalan con `mise install` (ver `mise.toml`).

```sh
cargo fmt --all                                         # formatear
cargo clippy --workspace --all-targets -- -D warnings   # lints (warnings = error)
cargo nextest run --workspace --no-tests=pass           # tests
cargo test --workspace --doc                            # doctests (nextest no los corre)
cargo deny check                                        # advisories, licencias, fuentes
cargo machete                                           # dependencias sin usar
cargo run -p clipbroad -- --help                        # correr el binario
```

**Antes de dar una tarea por terminada, todos estos comandos deben pasar.** El CI corre lo mismo en
Linux, macOS y Windows.

## Convenciones de código

- Código, identificadores, comentarios, mensajes de commit y PRs: **en inglés**. Documentación en `docs/`:
  en español.
- Edición 2024. Lints del workspace en `Cargo.toml` (`clippy::pedantic`, `unwrap_used`, `panic`,
  `unsafe_code = "deny"`). No silenciar lints con `#[allow]` sin un comentario que explique por qué.
- Errores: `thiserror` en los crates de librería, `anyhow` en `daemon`. Sin `unwrap()` fuera de tests;
  `expect()` solo para invariantes que indican un bug, con mensaje que diga cuál.
- Async con `tokio`. Logging con `tracing`. Nada de `println!` para diagnóstico.
- `unsafe`: solo si una API del SO no tiene binding seguro, con `#[allow(unsafe_code)]` local y un
  comentario `// SAFETY:` que justifique cada bloque.
- Código de plataforma siempre detrás del trait `Clipboard`; la lógica se prueba con el fake.

## Dependencias

- Antes de agregar un crate: verificar la versión actual (`cargo info <crate>`), que esté mantenido y
  que su licencia pase `cargo deny check`. Justificar la dependencia en el PR.
- Versiones compartidas en `[workspace.dependencies]`; los crates las heredan con `.workspace = true`.
- Preferir los crates ya decididos en `docs/DESIGN.md` (iroh, wl-clipboard-rs, arboard, ...).

## Guía de Rust

La skill `rust-skills` (`.claude/skills/rust-skills/SKILL.md`, reglas en `rules/`) tiene buenas
prácticas de Rust. Agentes sin soporte de skills pueden leer los archivos directamente. Prioridades:
1. Corrección y claridad primero: ownership, manejo de errores, async, API.
2. Las reglas de rendimiento y memoria (`mem-*`, `opt-*`, `perf-*`: SmallVec, arenas, etc.) **solo** en
   código caliente y medido. No agregar dependencias por sugerencia de esas reglas sin justificarlo.

Referencia para revisiones más profundas: [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines).

## Flujo de trabajo

- Una rama y un PR por unidad de trabajo, idealmente una fase o subfase de `docs/DESIGN.md`.
- Commits con [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, ...).
- El PR se mergea solo con el CI verde en los tres SO.
- Si una decisión de diseño cambia, actualizar `docs/DESIGN.md` en el mismo PR.
- No hacer push a `main` directamente.
