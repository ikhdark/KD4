#![crate_type = "lib"]

pub struct Server;

impl Server {
    pub fn wait_for_request_count(&self, count: usize) {
        let _ = count;
    }

    pub fn enabled(self, enabled: bool) -> Self {
        let _ = enabled;
        self
    }
}
