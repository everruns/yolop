pub const VERSION_DETAILS: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("YOLOP_GIT_SHA"),
    ", everruns-runtime ",
    env!("YOLOP_EVERRUNS_RUNTIME_VERSION"),
    ")"
);

pub const VERSION_LINE: &str = concat!(
    "yolop ",
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("YOLOP_GIT_SHA"),
    ", everruns-runtime ",
    env!("YOLOP_EVERRUNS_RUNTIME_VERSION"),
    ")"
);
