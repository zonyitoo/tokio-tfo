use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(unix)] {
        mod unix;
        pub use self::unix::*;
    } else if #[cfg(windows)] {
        mod windows;
        pub use self::windows::*;
    } else {
        compile_error!("Doesn't support TFO on the current platform");
    }
}
