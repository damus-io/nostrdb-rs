#[cfg(target_os = "windows")]
include! {"bindings_win.rs"}

#[cfg(not(target_os = "windows"))]
include! {"bindings_posix.rs"}
