use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct BacklightFixture {
    pub root: PathBuf,
    pub path: PathBuf,
}

impl BacklightFixture {
    pub fn new(maximum: u64) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "wluma-backlight-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let fixture = Self {
            path: root.join("panel"),
            root,
        };
        fixture.create(maximum);
        fixture
    }

    pub fn create(&self, maximum: u64) {
        std::fs::create_dir_all(&self.path).unwrap();
        std::fs::write(self.path.join("brightness"), format!("{}\n", maximum / 2)).unwrap();
        std::fs::write(self.path.join("max_brightness"), format!("{maximum}\n")).unwrap();
    }

    pub fn remove(&self) {
        std::fs::rename(&self.path, self.root.join("retired")).unwrap();
    }

    pub fn replace(&self, maximum: u64) {
        self.remove();
        self.create(maximum);
    }
}

impl Drop for BacklightFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
