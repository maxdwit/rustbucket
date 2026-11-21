#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (q, body) = data.split_at(data.len() / 2);
    let _ = rustbucket::sharing_api::fuzz_params(&String::from_utf8_lossy(q), data.first().is_some_and(|b| b % 2 == 0), body);
});
