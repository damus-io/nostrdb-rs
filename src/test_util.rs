use std::fs;

pub fn cleanup_db() {
    let _ = fs::remove_file("data.mdb");
    let _ = fs::remove_file("lock.mdb");
}
