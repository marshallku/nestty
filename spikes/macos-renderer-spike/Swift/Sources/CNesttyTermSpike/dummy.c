// SwiftPM requires at least one source file in the target so the
// clang module + linker settings actually get pushed to the final
// executable. Real symbols come from the linked Rust staticlibs.
void _nestty_term_spike_dummy(void) {}
