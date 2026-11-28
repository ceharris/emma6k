# CLAUDE.md — `emma6k-debugger`

A Tauri 2 desktop app (`emma6k-debugger`) that loads config from
`~/.emma/debugger/profiles/default/emulator.toml`, builds an `EmulatorSession` with an
injected `InternalPipeTransport` wired to its terminal window, and exposes the emulator to a
React/TypeScript frontend (`debugger/frontend/`) via `#[tauri::command]`s. UI preferences
(theme, exit-confirmation skip) are not profile-scoped and live in
`~/.emma/debugger/config/ui.toml` instead. One module per UI panel:

- **`cpu`** — `DebugCpu` (an enum of the 6502's and the 6809's `Cpu`; `CpuState` holds one),
  `DebugRegisters`, and the `with_processor!` macro that runs the same generic code on either
  arm; the session's `CpuDescription` (registers, flags, interrupt lines, stack, byte order),
  sent by `get_cpu_description` and as the `session-loaded` payload, and register access by
  name; the frontend's `CpuDescriptionContext` holds it and the panels render from it.
  `lib.rs`'s `FamilyConfig` picks the family from the profile's `cpu-variant`/`cpu` (none →
  6502) and builds that family's session. Free-running matches the enum once and runs
  `exec::run_from::<P>` for the concrete CPU (`disassembly::start_run`, `DebugProcessor`), so
  the run loop never goes through `dyn Processor`
- **`registers`** — register snapshot/edit
- **`cpu_bus`** — reset, IRQ and FIRQ assert/release, NMI trigger, cached bus-signal snapshot.
  The FIRQ control's source (`DEBUGGER_FIRQ`, only for a CPU with FIRQ) is an ordinary IRQ
  source added to the interrupt controller's FIRQ mask
- **`disassembly`** — run/stop/step-into/step-over/step-return, breakpoint CRUD, disassembly listing
- **`memory`** — paged reads/writes/fills/file loads
- **`stack`** — stack pointer, top, and the CPU's stack window (`Processor::stack_window`), with full addresses
- **`terminal`** — console byte-stream bridge and window visibility (toggleable window)
- **`trace`** — live-recorded execution trace, windowed reads
- **`watchpoints`** — loads/compiles `watchpoints.emw`, evaluates on demand, add/remove/edit/toggle with persistence
- **`theme`** — light/dark theme preference; also owns `UiConfig`/`ui.toml` persistence used by
  the exit-confirmation "Don't ask again" preference (set from `lib.rs`'s `confirm_exit`)
- **`menu`** — native File/Edit/Window/Help menu bar and Window-menu checkbox sync
- **`recent`** — recently-used profile list (`~/.emma/debugger/config/recent.toml`), recorded on every
  profile activation and shown in the File > Open Recent submenu
- **`profile`** — `--profile` CLI flag, profile directory resolution, `ensure_profile_dir` (seeds a
  new `default` profile from the bundled `emulator::config::templates` default (taliforth)
  template; seeds any other new profile by copying files from `default`), `list_templates`/
  `create_profile_dir_from_template` (New Profile's template picker: every family's templates,
  from `emma6k::family`, each tagged with its CPU), New/Open Profile commands, window-title
  sync

Devices requiring a byte-stream peer (VIA, MC6840, ACIAs) still use their configured
`Transport` independent of the debugger UI; only the console is special-cased to route
through the debugger's own terminal window.
