use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Shutdown {
    requested: watch::Sender<bool>,
}

impl Shutdown {
    pub fn new() -> Self {
        let (requested, _) = watch::channel(false);
        Self { requested }
    }

    pub fn request(&self) {
        self.requested.send_replace(true);
    }

    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.requested.subscribe();
        receiver
            .wait_for(|requested| *requested)
            .await
            .expect("shutdown sender lives as long as this future");
    }
}
