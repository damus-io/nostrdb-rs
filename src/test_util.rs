use std::fs;
use std::path::Path;

pub fn cleanup_db(path: &str) {
    let p = Path::new(path);
    let _ = fs::remove_file(p.join("data.mdb"));
    let _ = fs::remove_file(p.join("lock.mdb"));
}
