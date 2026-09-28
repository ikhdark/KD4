/// A queued response is not consumed merely because a channel accepted it.
/// Restore it if the receiving future is dropped before claiming the value.
pub(crate) struct Delivery<T> {
    value: Option<T>,
    restore: Option<Box<dyn FnOnce(T) + Send>>,
    claimed: Option<Box<dyn FnOnce() + Send>>,
}

impl<T> Delivery<T> {
    pub(crate) fn new(value: T, restore: impl FnOnce(T) + Send + 'static) -> Self {
        Self {
            value: Some(value),
            restore: Some(Box::new(restore)),
            claimed: None,
        }
    }

    pub(crate) fn on_claim(mut self, claimed: impl FnOnce() + Send + 'static) -> Self {
        self.claimed = Some(Box::new(claimed));
        self
    }

    #[expect(clippy::expect_used, reason = "claim consumes a delivery constructed with a value")]
    pub(crate) fn claim(mut self) -> T {
        self.restore = None;
        if let Some(claimed) = self.claimed.take() {
            claimed();
        }
        self.value.take().expect("unclaimed delivery")
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Delivery<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

impl<T> Drop for Delivery<T> {
    fn drop(&mut self) {
        if let (Some(value), Some(restore)) = (self.value.take(), self.restore.take()) {
            restore(value);
        }
    }
}
