use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    if let Ok(out_dir) = env::var("OUT_DIR") {
        let out_path = PathBuf::from(out_dir);
        // target/<profile>/build/ozan-<hash>/out -> 3 üst dizin = target/<profile>
        if let Some(profile_dir) = out_path.ancestors().nth(3) {
            let src = profile_dir.join("ozan_apo.dll");
            let dst = profile_dir.join("ozan-apo.dll");
            if src.is_file() {
                let _ = fs::copy(&src, &dst);
            }
        }
    }
    tauri_build::build()
}
