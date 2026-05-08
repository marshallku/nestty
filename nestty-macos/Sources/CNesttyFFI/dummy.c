// SwiftPM treats a C target with only headers as "header only" and skips
// linking. We need a real .c file (even if empty) so the target produces an
// object file that participates in the link graph — that's what carries the
// linker settings (-L<workspace>/target/release -lturm_ffi) onto the final
// Turm executable. Without this, the Rust archive never gets pulled in.
