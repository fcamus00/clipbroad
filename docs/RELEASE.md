# Releases

Un tag `v*` en GitHub dispara `.github/workflows/release.yml`, que compila los 3 binarios y publica un
GitHub Release con ellos.

## Cómo sacar un release

1. Subir la versión en `[workspace.package]` del `Cargo.toml` raíz (por ejemplo `0.2.0`) y correr
   `cargo check` para actualizar `Cargo.lock`. El build usa `--locked` y falla si el lock quedó viejo.
2. Abrir un PR con ese cambio y mergearlo con el CI verde.
3. Crear el tag sobre `main` y subirlo:

   ```sh
   git fetch origin
   git tag v0.2.0 origin/main
   git push origin v0.2.0
   ```

4. Seguir el workflow con `gh run watch` (o en la pestaña Actions). Al terminar, el release está en
   `gh release view v0.2.0`.

Un tag con sufijo (`v0.2.0-rc.1`) crea un **pre-release**. La versión del binario no lleva el sufijo:
`v0.2.0-rc.1` espera que `clipbroad --version` diga `clipbroad 0.2.0`.

## Qué hace el workflow

- **build** (Linux x86_64, macOS Apple Silicon, Windows x86_64): `cargo build --release --locked`, y
  comprueba que `clipbroad --version` coincida con el tag. Si no coincide, falla y no publica nada.
- Empaqueta el binario con `README.md`, `LICENSE-APACHE` y `LICENSE-MIT` en una carpeta
  `clipbroad-<tag>-<plataforma>/`, como `.tar.gz` (Linux, macOS) o `.zip` (Windows).
- **release**: junta los 3 archivos, escribe `SHA256SUMS` y crea el release con `gh release create`
  (notas generadas a partir de los PRs mergeados).

## Si algo falla

El release se crea en el último paso; si falla antes, no se publica nada. Para reintentar con el mismo
número: arreglar en `main`, borrar el tag y crearlo de nuevo.

```sh
git push origin :refs/tags/v0.2.0 && git tag -d v0.2.0
gh release delete v0.2.0 --yes   # solo si el release llegó a crearse
```

## Instalar desde un release

```sh
gh release download v0.2.0 --repo fcamus00/clipbroad --pattern '*linux-x86_64*' --pattern SHA256SUMS
grep linux-x86_64 SHA256SUMS | sha256sum -c      # en macOS: shasum -a 256 -c
tar -xzf clipbroad-v0.2.0-linux-x86_64.tar.gz
./clipbroad-v0.2.0-linux-x86_64/clipbroad --version
```

En macOS el binario no está firmado ni notarizado (requiere cuenta de Apple Developer; está en el
backlog). Si el archivo se bajó con un navegador, macOS lo marca en cuarentena y Gatekeeper no deja
correrlo. Quitar la marca después de extraer:

```sh
xattr -d com.apple.quarantine clipbroad
```

`gh release download` normalmente no pone la marca. Si `xattr` dice "No such xattr", no hay nada que
hacer.

## Por qué un workflow propio y no cargo-dist

Se evaluó [`cargo-dist`](https://github.com/axodotdev/cargo-dist) (0.32). Hace lo mismo y más
(instaladores shell/PowerShell, Homebrew, MSI), pero genera un workflow de varios cientos de líneas que
no se edita a mano, agrega configuración propia (`dist-workspace.toml`) y hay que regenerarlo al
actualizar la herramienta. Hoy solo se necesitan 3 archivos y checksums: el workflow propio son unas 90
líneas que se leen de una vez y usan las mismas acciones que el CI. Si más adelante hacen falta
instaladores o Homebrew, conviene revisar `cargo-dist` de nuevo.
