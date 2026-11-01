# macOS Native Emoji Picker

## Goal and agreed behavior

Support Fn+E, Control+Command+Space, and Edit > Emoji & Symbols in Neovide.
The Globe key also opens the picker when configured in macOS Keyboard settings.
Deliver selected Unicode exactly once through the receiving window's existing
text-input path. Neovide must not force a mode change or enable IME against
`g:neovide_input_ime=false`.

The scope is opening the native picker and inserting selected Unicode. Preceding-word
replacement, surrounding-text APIs, and Dictation enhancements are separate work.
Retain the project's deployment target and let shortcut availability follow macOS
and hardware capabilities. No new settings, RPC methods, or public input types are needed.

Planning baseline: Neovide `66b945e2665bd648a8e7c56133f0cd906adcab98` (0.17.1),
winit 0.30.13. Research date: 2026-11-01.

## Research

- Apple documents [Fn+E and Control+Command+Space](https://support.apple.com/en-us/102650)
  as Character Viewer shortcuts. AppKit provides
  [`orderFrontCharacterPalette:`](https://developer.apple.com/documentation/appkit/nsapplication/orderfrontcharacterpalette%28_%3A%29?language=objc)
  to open the native UI.
- [Neovide #1493](https://github.com/neovide/neovide/issues/1493) remains open.
  Its history distinguishes opening the picker from delivering selected text.
- [winit #3342](https://github.com/rust-windowing/winit/issues/3342) documents
  `insertText:replacementRange:` dropping picker selections when no composition is active.
  The pinned 0.30.13 implementation commits only when marked text and active IME state exist.
- Upstream [winit PR #4639](https://github.com/rust-windowing/winit/pull/4639)
  merged on 2026-10-29 as
  [`ab2fe0cd12ff83fc287d9448ef1cdc4074e8bfeb`](https://github.com/rust-windowing/winit/commit/ab2fe0cd12ff83fc287d9448ef1cdc4074e8bfeb).
  It distinguishes standalone text callbacks from callbacks inside keyboard interpretation.
  The 0.30 branch still contained the original behavior when researched.
- [The proposed 0.30 backport](https://github.com/rust-windowing/winit/pull/4749)
  and [the standalone-commit lifecycle follow-up](https://github.com/rust-windowing/winit/pull/4784#issuecomment-6306483994)
  also show why an external composition commit must return to the ground state:
  there is no confirming key to suppress, and leaving `Committed` can swallow the next key.
- An isolated AppKit probe on macOS 27.2 observed that installing a standard Edit menu
  produces native Emoji & Symbols entries, including Fn+E and Control+Command+Space
  aliases. This was menu inspection, not end-to-end Neovide validation.

## Implementation

### Native menu

Install a standard Edit menu between the application and Window menus during existing
menu construction. Let AppKit populate Emoji & Symbols and manage the native aliases.
Keep dispatch in AppKit rather than adding Fn parsing to Neovide's keyboard manager.
Check that initial focus and subsequent window creation retain a single visible picker item.

### winit backport

Carry the focused backport in `mevanlc/winit`, branch `macos-emoji-picker-0.30`.

- Base: winit 0.30.13, `e9809ef54b18499bb4f2cac945719ecc2a61061b`.
- Canonical fix: upstream `ab2fe0cd12ff83fc287d9448ef1cdc4074e8bfeb` (PR #4639).
- Pinned backport:
  [`f2281c2dc23dfb88c4fce6e679051ce25fb28d75`](https://github.com/mevanlc/winit/commit/f2281c2dc23dfb88c4fce6e679051ce25fb28d75).

Track entry into `interpretKeyEvents`, restoring the previous flag to support nesting.
Keep ordinary keyboard input and composition behavior intact. While IME is allowed,
standalone nonempty, non-control text produces an IME commit, preceded by `Ime::Enabled`
when necessary. An external commit that finishes composition clears marked text and
returns to the ground state without suppressing the next unrelated key.

Use a full immutable commit hash in Neovide's existing `[patch.crates-io]` table.
Retain the exact 0.30.13 requirement and existing features. Neovide and glutin-winit
must resolve the same winit package. Record the resulting lockfile source changes.
The fork uses the published `dpi` dependency, so Neovide retains its existing
0.1.2 version and the lockfile changes only winit's source.

The fork includes a macOS callback regression example, runnable with
`cargo run --locked --example character_viewer_regression`. It uses a hidden winit
window and real AppKit callbacks without opening the picker or changing input settings.

### Commit formatting and caret position

For macOS IME commits, preserve raw Unicode and format Neovim input without held
modifiers or case conversion. Escape each literal `<` as `<lt>`. Keep physical
keyboard shortcut formatting and the existing commit command/Lua callback interfaces.

Keep AppKit's caret rectangle current from the first rendered frame, before composition
begins. Retain the existing coordinate conversion and cache. Publishing the rectangle
must not enable IME. Other platforms keep their existing composition gate.

Document entry points and the IME requirement in the feature and configuration guides.

## Validation plan

1. Check callback delivery in the winit regression example: fresh standalone Unicode,
   next-key delivery, repeated commits, disabled IME, empty/control callbacks,
   preedit, and external composition completion. Exercise real in-key composition
   through the packaged GUI rather than constructed AppKit events.
2. Test macOS commit formatting with emoji, ZWJ sequences, flags, skin tones,
   combining marks, embedded `<`, and held Control/Command/Option/Shift modifiers.
   Confirm ordinary keyboard shortcuts retain their formatting.
3. Run `cargo fmt --all --check`, `cargo check --locked`,
   `cargo nextest run --locked`, and relevant Clippy checks. Confirm one resolved
   winit source. Linux/Windows validation requires those platforms or their CI jobs.
4. Build the release binary explicitly because the packaging script reuses an
   existing binary:

   ```sh
   cargo build --release --locked
   GENERATE_BUNDLE_APP=true GENERATE_DMG=false ./macos-builder/run
   codesign --force --deep --sign - target/release/bundle/osx/Neovide.app
   ```

5. Test the packaged app with a clean Neovim configuration:
   - Fresh-session invocation through each entry point, with no preceding composition.
   - One picker, no shortcut text leakage, insertion exactly once, and no lost next key.
   - Cancellation and ordinary `e`/existing shortcuts.
   - Insert, command-line, and terminal input, preserving Neovim's mode behavior.
   - Multiple windows, native tabs, fullscreen, and different display scales.
   - Disabled IME, re-enabling IME, and existing dead-key/CJK composition.

### Execution results

Validated on macOS 27.2, Apple Silicon, with Neovim 0.12.6 and the repository's
`nightly-2026-10-18` Rust toolchain.

| Check | Result |
| --- | --- |
| winit native callback example | All 8 cases passed |
| winit library tests | All 7 tests passed with nextest |
| winit formatting and Clippy | Passed, including the fork's all-targets/all-features commit hook |
| Neovide `cargo check --locked` | Passed |
| Neovide `cargo nextest run --locked` | All 240 tests passed |
| Neovide CI-equivalent Clippy | `cargo clippy --locked --all-targets -- -D warnings` passed |
| Rust formatting, website Markdown, fixture Python lint | Passed |
| Release build and app signature verification | Passed |
| Dependency identity | One winit shared by Neovide and glutin-winit; existing dpi 0.1.2 retained |

The user physically tested Fn+E in **Neovide Emoji Test** and confirmed that the
native picker opens. The fixture recorded one native selection, `⚠️`, with identical
raw and formatted strings in Insert mode. Typing `x` afterward produced `x⚠️x`
with one recorded commit, confirming that the next key was not lost or duplicated.
The native Edit menu exposed one visible Emoji & Symbols item with the
`orderFrontCharacterPalette:` accessibility identifier.

Real GUI Option+E followed by E produced one `é` commit. The following were also verified:

- Insert mode: `é`, and `éx` after re-enabling IME and typing the next key.
- Command-line mode: command-line text `éx`, mode `c`, one commit.
- Terminal mode with `/bin/cat`: first line `éx`, mode `t`, one commit.
- Normal mode: buffer text stayed `unchanged`, mode stayed `n`, and one commit arrived.
- With `g:neovide_input_ime=false`, dead-key input produced no composition commits;
  ordinary `x` typing still arrived. Re-enabling restored composition.

The optional all-features check fails because `MetalSkiaRenderer` does not implement
`tracy_create_gpu_context` for GPU profiling. The identical error was reproduced with
`cargo check --locked --all-features` in a temporary, untouched worktree at baseline
`66b945e`. That worktree was removed afterward. This does not affect the default-feature
build or the project's normal Clippy command.

Not independently verified end to end: Control+Command+Space and menu selection,
Globe-only invocation, picker cancellation, multiple windows/native tabs, fullscreen,
different display scales, and a real CJK input source. The native aliases were observed
in the planning AppKit probe; automated inspection of the system input-method popup
was unreliable. Linux and Windows builds/CI were not run in this macOS workspace.

The signed application is `target/release/bundle/osx/Neovide.app`. The temporary test
copy uses bundle ID `com.neovide.neovide.emoji-test` to keep its backend separate from
existing Neovide sessions. Reproduce the fixture with:

```sh
uv run devdocs/macos_emoji_probe.py launch
uv run devdocs/macos_emoji_probe.py reset --mode insert
# Invoke the native picker and select an emoji in Neovide Emoji Test.
uv run devdocs/macos_emoji_probe.py snapshot
uv run devdocs/macos_emoji_probe.py close
```

The helper records raw/formatted commits and mode without changing the original
commit handler's input behavior. It uses Neovide's existing `:NeovideFocus` command
when resetting the fixture, because native text services need the app to own foreground
input. Its scratch buffers and terminal jobs are closed with the fixture.

## Removing the temporary fork pin

Once an official compatible winit release contains this fix, replace the fork pin
with that release and repeat the same validation. Do not adopt winit's newer API
as part of this focused backport.
