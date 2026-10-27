# NIH-plug: GPUI support

This provides an adapter to create [GPUI](https://gpui.rs) based GUIs with
NIH-plug, using the [GPUI Standalone
Edition](https://github.com/futureboard/gpui-se) fork. That fork packages GPUI as
a set of independent crates instead of shipping it as part of the Zed editor,
which is what makes it usable as a plugin GUI dependency in the first place.

See [`plugins/examples/gain_gui_gpui`](../plugins/examples/gain_gui_gpui) for a
complete plugin using this adapter.

## Usage

Write a regular GPUI [`Render`] implementation and hand it to
`create_gpui_editor()`. The `EditorContext` it passes to your build function is
how the GUI reads and writes parameters:

```rust
struct GainEditor {
    context: EditorContext,
    params: Arc<GainParams>,
}

impl Render for GainEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let setter = self.context.setter();

        div().child(format!("{} dB", util::gain_to_db(self.params.gain.value())))
    }
}

// In `Plugin::editor()`
create_gpui_editor(
    self.params.editor_state.clone(),
    move |context, _window, cx| {
        let context = context.clone();
        let params = params.clone();

        cx.new(|_| GainEditor { context, params })
    },
)
```

## How it works

GPUI is built to be an application framework rather than an embeddable widget
toolkit, so this adapter has to work around two things:

- **GPUI owns its event loop.** `Application::run()` only returns once the
  application quits, and a plugin can't take over the host's main thread. This
  adapter therefore runs a single process-wide GPUI application on a dedicated
  thread and drives it by sending it closures. That thread is started lazily when
  the first editor is opened and is deliberately never shut down, because GPUI's
  Windows backend calls `ExitProcess()` when its event loop terminates, which
  would take the entire DAW down with it.
- **GPUI only creates top level windows.** Plugin editors have to live inside a
  window owned by the host. After GPUI creates its window, this adapter reparents
  the underlying native window into the host's window using the platform's own
  APIs (`ReparentWindow` on X11, `SetParent` on Windows).

See the `host` and `embed` modules for the details.

## Platform support

|Platform|Status|
|---|---|
|Windows|Supported|
|Linux (X11)|Supported|
|Linux (Wayland)|Use XWayland; hosts hand out an X11 window and Wayland has no window reparenting|
|macOS|**Not supported**|

macOS is not supported because GPUI's backend drives `NSApplication` and must run
on the process' main thread, which the plugin host already owns. Until GPUI gains
a way to cooperatively integrate with an existing run loop there is no correct way
to host it from a plugin, so opening an editor on macOS logs an error and does
nothing rather than corrupting the host's state.

## Known issue: gpui-se needs two manifest fixes

At the time of writing, `futureboard/gpui-se` does **not** build on Linux, which
means neither does this crate. The Linux backend has two version mismatches in the
fork's workspace `Cargo.toml`, both independent of NIH-plug:

1. `accesskit_unix = "0.17"` resolves to a release that depends on `accesskit
   0.21`, while the workspace pins `accesskit = "0.24"`. The two versions of the
   crate's types don't unify, producing trait bound and type mismatch errors in
   `gpui_linux`. `accesskit_unix 0.21` is the release that depends on `accesskit
   0.24`.
2. The code uses `ashpd::Uri`, which only exists from `ashpd 0.13` onwards, while
   the workspace pins `ashpd = "0.11"`. `ashpd 0.13` also renamed the `async-std`
   feature to `async-io` and moved the individual portals behind features, which
   the `frontend` feature enables as a group.

Applying both of these to `gpui-se`'s workspace `Cargo.toml` makes `gpui_linux`
compile:

```diff
-accesskit_unix = "0.17"
+accesskit_unix = "0.21"
-ashpd = { version = "0.11", default-features = false, features = ["async-std"] }
+ashpd = { version = "0.13", default-features = false, features = ["async-io", "frontend"] }
```

Until the fork carries these fixes, you can build against a locally patched
checkout by adding this to the workspace root `Cargo.toml`:

```toml
[patch."https://github.com/futureboard/gpui-se.git"]
gpui = { path = "/path/to/gpui-se" }
gpui_linux = { path = "/path/to/gpui-se/crates/gpui_linux" }
```

## A note on dependencies

This crate depends on `gpui` with `default-features = false` and on the platform
backend crates directly rather than through the `gpui_platform` convenience crate.
Both are deliberate: GPUI's default features enable `font-kit`, which is only used
by the macOS backend but which pulls in a crate that links against `fontconfig`,
and `gpui_platform` depends on `gpui` with its default features enabled. Cargo
allows only one crate per dependency graph to link against a given native library.

For the same reason `nih_plug_iced` is not part of this repository's Cargo
workspace. See the comment above the workspace `members` list in the root
`Cargo.toml`.
