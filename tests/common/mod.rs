use std::path::Path;
use std::process::{Command, Output};

pub struct Drip {
    pub bin: String,
    pub data_dir: tempfile::TempDir,
    pub session_id: String,
}

impl Drip {
    pub fn new() -> Self {
        let bin = env!("CARGO_BIN_EXE_drip").to_string();
        let data_dir = tempfile::tempdir().expect("tempdir");
        // Stable session id so successive invocations share state.
        let session_id = format!("test-{}", uniq_suffix());
        Self {
            bin,
            data_dir,
            session_id,
        }
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("DRIP_DATA_DIR", self.data_dir.path());
        c.env("DRIP_SESSION_ID", &self.session_id);
        // Bash-hook tiny-output bypass would mask transformation
        // correctness assertions on small fixtures — disable here so
        // tests can byte-compare the rendered pipeline output.
        c.env("DRIP_PIPELINE_BYPASS_BYTES", "0");
        // Pin compression body floor so fixtures with short helper
        // bodies stay deterministic across product-default tweaks.
        c.env("DRIP_COMPRESS_MIN_BODY", "4");
        c
    }

    /// Same data dir, different session id — used to assert that
    /// lifetime aggregation crosses session boundaries.
    pub fn cmd_in_session(&self, session_id: &str) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("DRIP_DATA_DIR", self.data_dir.path());
        c.env("DRIP_SESSION_ID", session_id);
        c.env("DRIP_PIPELINE_BYPASS_BYTES", "0");
        c.env("DRIP_COMPRESS_MIN_BODY", "4");
        c
    }

    pub fn read(&self, file: &Path) -> Output {
        self.cmd()
            .arg("read")
            .arg(file)
            .output()
            .expect("drip read failed to launch")
    }

    pub fn read_stdout(&self, file: &Path) -> String {
        let o = self.read(file);
        assert!(
            o.status.success(),
            "drip read failed: stderr={}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    pub fn meter(&self) -> String {
        let o = self
            .cmd()
            .arg("meter")
            .output()
            .expect("drip meter failed to launch");
        assert!(o.status.success(), "drip meter failed");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    pub fn reset(&self) {
        let o = self
            .cmd()
            .arg("reset")
            .output()
            .expect("drip reset failed to launch");
        assert!(o.status.success(), "drip reset failed");
    }
}

fn uniq_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos}-{}", std::process::id())
}
