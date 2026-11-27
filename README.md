# Wayfold

Wayfold is a file manager for macOS and an alternative to Finder. It brings folders, file previews and a terminal into one workspace, for people who regularly organize files or work across several locations.

Tabs keep folders close at hand, and split panes let you browse two locations side by side. You can switch between list, icon and column views, keep favorite folders in the sidebar, and use keyboard shortcuts to move around. The aim is to make everyday file work feel direct, with more tools available when you need them.

Wayfold is built in Rust with GPUI, using a native application interface. It is developed for macOS; Linux support may be considered later.

**This is an intermediate development version.** The application is running, but behavior and design are still being refined. In-app verification, accessibility checks, performance measurements and release preparation remain in progress. The specification describes a wider scope than this introduction; it should not be read as a list of finished, release-ready features.

To try the current version, build it from source on a Mac. You will need Rust through rustup and the Xcode/Metal build tools. The repository pins its Rust toolchain in `rust-toolchain.toml`.

From the repository folder, run:

```sh
cargo run -p wayfold
```

Start with a folder of disposable files while evaluating this development build. File operations act on real files.

A few shortcuts to get started:

| Shortcut | Action |
| --- | --- |
| ⌘T | Open a new tab |
| ⌘⇧D | Toggle split panes |
| ⌘⇧G | Go to a folder by path |
| Space | Toggle the selected item's preview |
| ⌘K | Open the command palette |
| F4 | Toggle the terminal |

Feedback about confusing interactions, unexpected behavior and everyday workflows is welcome. Include what you were trying to do, what happened and your macOS version when reporting a problem.
