pub const VERSION_DETAILS: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("YOLOP_GIT_SHA"),
    ", everruns-core ",
    env!("YOLOP_EVERRUNS_CORE_VERSION"),
    ")"
);

pub const VERSION_LINE: &str = concat!(
    "yolop ",
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("YOLOP_GIT_SHA"),
    ", everruns-core ",
    env!("YOLOP_EVERRUNS_CORE_VERSION"),
    ")"
);
