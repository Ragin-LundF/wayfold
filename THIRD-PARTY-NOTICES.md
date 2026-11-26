# Third-party notices

Waypoint is built on open-source components. The full list of crates and versions is pinned in
`Cargo.lock`; the main runtime dependencies and their licenses are listed in
`spec_impl/finder-spec/05-implementation.md` ("Runtime dependencies and licenses").

The packaged app must ship this file and the `licenses/` folder.

## unRAR (RAR extraction, F08)
Waypoint extracts RAR archives with the `unrar` / `unrar_sys` crates (0.5.8), which bundle the
unRAR source by Alexander Roshal. Waypoint never creates RAR archives. The unRAR source may not be
used to develop a RAR (WinRAR) compatible archiver. Full license text: `licenses/unRAR-license.txt`.

## Lucide icons
Icons from Lucide, bundled through `gpui-kit-assets` 0.11.0 (ISC license). Full text:
`licenses/Lucide-LICENSE.txt`.

## Apache-2.0 components
GPUI (`gpui-pre` 0.3.14, a snapshot of the Zed editor's GPUI), `gpui-component`, `gpui-base` and
`gpui-kit` 0.11.0, `alacritty_terminal` 0.26 and others are licensed under the Apache License 2.0
(https://www.apache.org/licenses/LICENSE-2.0). Their copyright notices are kept in their source
distributions as published on crates.io.

## MIT / other permissive components
`portable-pty`, `objc2*`, `plist`, `regex`, `ammonia`, `csv`, `imagesize`, `zip`, `tar`, `flate2`,
`zstd`, `sevenz-rust2`, `blake3`, `notify` (CC0-1.0 / Artistic-2.0) and their dependencies are
under MIT, Apache-2.0, BSD, ISC, Zlib, Unlicense or CC0 terms as declared in each crate's manifest.
