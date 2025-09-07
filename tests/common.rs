use std::sync::Once;

pub fn init_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        blipmq::logging::init_logging();
    });
}
