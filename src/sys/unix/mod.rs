use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android"))] {
        mod linux;
        pub use self::linux::*;
    } else if #[cfg(any(target_os = "freebsd",
                        target_os = "openbsd",
                        target_os = "netbsd",
                        target_os = "dragonfly",
                        target_os = "macos",
                        target_os = "ios",
                        target_os = "watchos",
                        target_os = "tvos"))] {
        mod bsd;
        pub use self::bsd::*;
    } else {
        compile_error!("Doesn't support TFO on the current platform");
    }
}
