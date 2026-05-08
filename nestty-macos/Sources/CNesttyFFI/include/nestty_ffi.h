// nestty_ffi.h — C declarations for symbols exported by the nestty-ffi staticlib.
//
// Hand-maintained to match nestty-ffi/src/lib.rs. The crate has no cbindgen
// step yet because the surface is small and the spike doesn't justify the
// build-system overhead. Keep this file in lockstep with the Rust source —
// any new `extern "C"` symbol there needs a declaration here, with the same
// ownership/safety contract documented.

#ifndef NESTTY_FFI_H
#define NESTTY_FFI_H

#ifdef __cplusplus
extern "C" {
#endif

/// Returns a NUL-terminated static version string. DO NOT free.
const char *nestty_ffi_version(void);

/// Echo a JSON string back with an `echoed_at` timestamp added. Returns a
/// heap-allocated NUL-terminated string the caller MUST free with
/// `nestty_ffi_free_string`. Returns NULL on error; call `nestty_ffi_last_error`
/// for the message.
char *nestty_ffi_call_json(const char *input);

/// Free a string previously returned by a nestty-ffi function. Pass NULL is OK.
void nestty_ffi_free_string(char *s);

/// Returns the most recent error message recorded on the calling thread,
/// or NULL if none. The pointer is borrowed (do NOT free) and is invalidated
/// by the next FFI call on the same thread.
const char *nestty_ffi_last_error(void);

// ---------------------------------------------------------------------------
// PR 5c — Engine FFI
//
// Wraps nestty_core::trigger::TriggerEngine. Hand-maintained mirror of the
// `extern "C"` symbols in nestty-ffi/src/lib.rs's "PR 5c" block. Add a
// declaration here when adding a Rust symbol; both files must stay in sync.
// ---------------------------------------------------------------------------

/// Opaque engine handle. Created by nestty_engine_create, freed by
/// nestty_engine_destroy. Pass through every other engine call.
typedef struct EngineHandle EngineHandle;

/// Action callback signature. Engine calls this for each trigger that
/// matches a dispatched event. user_data is whatever the host passed to
/// nestty_engine_set_action_callback. action_name and params_json are
/// borrowed — host must NOT free them.
typedef void (*nestty_action_callback)(
    void *user_data,
    const char *action_name,
    const char *params_json
);

EngineHandle *nestty_engine_create(void);
void nestty_engine_destroy(EngineHandle *handle);

/// Install / replace the action callback. NULL clears the slot.
void nestty_engine_set_action_callback(
    EngineHandle *handle,
    nestty_action_callback callback,
    void *user_data
);

/// Replace the trigger list. JSON shape mirrors TOML [[triggers]] entries.
/// Returns count loaded on success, -1 on parse error (use nestty_ffi_last_error).
int nestty_engine_set_triggers(EngineHandle *handle, const char *triggers_json);

/// Dispatch an event into the engine. Returns # of triggers fired, or -1
/// on bad input.
///
/// `source` controls the trust-boundary stamp on the synthesized Event.
/// Pass "nestty.action" for registry-synthesized completion events to
/// satisfy await-promotion (see nestty_core::action_registry::
/// COMPLETION_EVENT_SOURCE). Pass NULL to default to "macos.eventbus".
///
/// `context_json` is an optional `Context` snapshot for `{context.X}`
/// interpolation + condition evaluation. Wire shape matches
/// `nestty_core::context::Context` serde
/// ({active_panel: string?, active_cwd: string?}). NULL → engine
/// dispatches with no context (interpolation tokens preserved literally,
/// condition references resolve to null).
int nestty_engine_dispatch_event(
    EngineHandle *handle,
    const char *event_kind,
    const char *source,
    const char *context_json,
    const char *payload_json
);

/// Diagnostic: number of triggers currently loaded.
int nestty_engine_count_triggers(EngineHandle *handle);

#ifdef __cplusplus
}
#endif

#endif // NESTTY_FFI_H
