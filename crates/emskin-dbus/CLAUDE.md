# emskin-dbus — session-bus proxy for nested Wayland compositors

Zero smithay deps. Reusable by any nested compositor (cage, wio, niri-in-plasma, …) in the same spirit as sibling `emskin-clipboard`.

## Scope matrix

| Feature | Phase 1 | Phase 2 |
|---|---|---|
| Transparent pass-through broker (SASL, Hello, serial remap) | ✅ | |
| ctl-socket protocol (`EmskinToProxy` / `ProxyToEmskin`) | ✅ | |
| `SetCursorRect` / `SetCursorLocation` arg rewrite → closes emskin#55 | ✅ | |
| `RequestName` local-own interception → closes emskin#60 | | ✅ |
| Merged `ListNames` / `NameOwnerChanged` view | | ✅ |
| Policy config (`passthrough` / `local-own` / `deny`) | | ✅ |

## Architecture

```
emskin-dbus-proxy (bin) ── launched by emskin ──────────┐
        │                                               │
        │ uses                                          │ ctl-socket (JSON)
        ▼                                               ▲
emskin-dbus (lib)                                       │
        │                                               │
        │ listens bus-socket                            │
        ▼                                               │
  per-child broker ──── upstream ──── host session bus  │
        │                                               │
        │ pushes focus/rect ←───────── EmskinToProxy ───┘
        ▼
  rule engine (arg rewrite)
```

## Invariants

- `ctx` (u64, minted by emskin) is the only stable handle. Proxy resolves `bus_unique_name → pid (SO_PEERCRED) → ctx (ClientBorn)`.
- Rectangles pushed through ctl-socket are in **emskin-winit-local coordinates** (not host-screen-absolute — emskin can't observe its own host position through Wayland). Host compositor composes the proxy-rewritten coords with emskin's winit surface position when drawing the fcitx5 popup.
- Same JSON length-prefix codec as emskin's existing Emacs IPC — no new wire format.
- `Ready` is the startup barrier: emskin must not spawn any child that depends on `DBUS_SESSION_BUS_ADDRESS` before it arrives.

## Non-goals

- No high-level `Proxy` / `ObjectServer`-style DBus API. This is a broker, not a service.
- No activation fork-exec logic in the proxy — all activation stays on the host bus (portal, notifications, keyring, fcitx all belong there).
