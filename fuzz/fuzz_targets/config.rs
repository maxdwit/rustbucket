#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rustbucket::config::Config::from_toml(&String::from_utf8_lossy(data));
});
