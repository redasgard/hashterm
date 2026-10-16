# Bundled colour schemes

Each file is one scheme: `name`, `dark`, `foreground`, `background`, optional
`cursor` and `selection`, and either an empty or a 16-colour ANSI `palette`
(black, red, green, yellow, blue, magenta, cyan, white, then the bright row).

Palettes were taken on 2026-10-15 from
[mbadolato/iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes)
(MIT licence, copyright Mark Badolato), which collects them from their original
authors (Tomorrow Night, Dracula, Nord, Solarized, Gruvbox, Catppuccin, Tokyo
Night, One Half, Atom One). The upstream collection is MIT-licensed; the
per-scheme licences of the originals were not audited individually and the
author credits are the upstream collection's, not checked here. If an author objects,
delete the file and its entry in `crates/hashterm-core/src/scheme.rs`.

Your own schemes: put a file of the same shape in
`~/.config/hashterm/schemes/NAME.toml` and set `scheme = "NAME"`.
