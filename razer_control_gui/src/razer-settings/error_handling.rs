pub trait Crash {
    type Value;
    fn or_crash(self, msg: impl AsRef<str>) -> Self::Value;
}

impl<T> Crash for Option<T> {
    type Value = T;
    fn or_crash(self, msg: impl AsRef<str>) -> Self::Value {
        match self {
            Some(v) => v,
            None => crash_with_msg(msg),
        }
    }
}

impl<T, E> Crash for Result<T, E> {
    type Value = T;
    fn or_crash(self, msg: impl AsRef<str>) -> Self::Value {
        match self {
            Ok(v) => v,
            Err(_) => crash_with_msg(msg),
        }
    }
}

pub fn crash_with_msg(msg: impl AsRef<str>) -> ! {
    eprintln!("FATAL: {}", msg.as_ref());
    std::process::exit(1);
}

pub fn setup_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Application panicked: {info}");
    }));
}
