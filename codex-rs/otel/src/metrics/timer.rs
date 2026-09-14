use crate::metrics::MetricsClient;
use crate::metrics::error::Result;
use std::time::Instant;

#[derive(Debug)]
pub struct Timer {
    name: String,
    tags: Vec<(String, String)>,
    client: MetricsClient,
    start_time: Instant,
    completed: bool,
}

impl Drop for Timer {
    fn drop(&mut self) {
        if !self.completed
            && let Err(e) = self.record(&[])
        {
            tracing::error!("metrics client error: {}", e);
        }
    }
}

impl Timer {
    pub(crate) fn new(name: &str, tags: &[(&str, &str)], client: &MetricsClient) -> Self {
        Self {
            name: name.to_string(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            client: client.clone(),
            start_time: Instant::now(),
            completed: false,
        }
    }

    /// Record a checkpoint; dropping the timer still records its full lifetime.
    /// Additional tags override the tags supplied at construction.
    pub fn record(&self, additional_tags: &[(&str, &str)]) -> Result<()> {
        let mut tags = Vec::with_capacity(self.tags.len() + additional_tags.len());
        tags.extend(self.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        tags.extend(additional_tags);
        self.client
            .record_duration(&self.name, self.start_time.elapsed(), &tags)
    }

    /// Complete the timer without recording again on drop, even if recording fails.
    pub fn finish(mut self, additional_tags: &[(&str, &str)]) -> Result<()> {
        self.completed = true;
        self.record(additional_tags)
    }
}
