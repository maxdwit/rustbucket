#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rustbucket::search::fuzz_search_request(data);
    let _ = rustbucket::search::fuzz_report_body(data);
});
