use {
    histogram::Histogram,
    log::*,
    std::{
        sync::RwLock,
        time::{Duration, Instant},
    },
};

#[derive(Debug)]
pub struct LoadAccountsHistogram {
    histogram_1_minute: Inner,
    histogram_1_hour: Inner,
}

impl LoadAccountsHistogram {
    #[must_use]
    pub fn new() -> Self {
        let start = Instant::now();
        let histogram_1_minute = Histogram::builder()
            .build()
            .expect("build 1 minute histogram");
        error!(
            "bprumo DEBUG: LoadAccountsHistogram, time to build 1 minute histogram: {:?}",
            start.elapsed()
        );

        let start = Instant::now();
        let histogram_1_hour = Histogram::builder()
            .build()
            .expect("build 1 hour histogram");
        error!(
            "bprumo DEBUG: LoadAccountsHistogram, time to build 1 hour histogram: {:?}",
            start.elapsed()
        );

        Self {
            histogram_1_minute: Inner {
                histogram: histogram_1_minute,
                previous_submit: RwLock::new(Instant::now()),
                submit_interval: Duration::from_secs(60),
                datapoint_name: "load_accounts_histogram-1_minute",
            },
            histogram_1_hour: Inner {
                histogram: histogram_1_hour,
                previous_submit: RwLock::new(Instant::now()),
                submit_interval: Duration::from_secs(60 * 60),
                datapoint_name: "load_accounts_histogram-1_hour",
            },
        }
    }

    /// Records `sample`
    ///
    /// Will be included in the next submission
    pub fn record(&self, sample: Duration) {
        let sample_ns = sample.as_nanos().try_into().expect("sample fits in u64");
        self.histogram_1_minute.record(sample_ns);
        self.histogram_1_hour.record(sample_ns);
    }

    /// Submits datapoint if enough time has passed since previous submission
    pub fn maybe_submit(&self) {
        self.histogram_1_minute.maybe_submit();
        self.histogram_1_hour.maybe_submit();
    }
}

impl Default for LoadAccountsHistogram {
    fn default() -> Self {
        Self::new()
    }
}

struct Inner {
    histogram: Histogram,
    previous_submit: RwLock<Instant>,
    submit_interval: Duration,
    datapoint_name: &'static str,
}

impl Inner {
    fn record(&self, sample: u64) {
        self.histogram
            .increment(sample, 1)
            .expect("increment histogram");
    }

    fn maybe_submit(&self) {
        if self.previous_submit.read().unwrap().elapsed() >= self.submit_interval {
            self.submit();
        }
    }

    fn submit(&self) {
        let mut previous_submit = self.previous_submit.write().unwrap();
        let duration = previous_submit.elapsed();
        let percentiles = [
            10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0, 99.9, 99.99, 99.999, 100.0,
        ];
        let percentiles = self
            .histogram
            .percentiles(&percentiles)
            .expect("histogram percentiles");

        datapoint_info!(
            self.datapoint_name,
            ("duration_ns", duration.as_nanos(), i64),
            ("load_time_ns_p10", percentiles[0].bucket().count(), i64),
            ("load_time_ns_p25", percentiles[1].bucket().count(), i64),
            ("load_time_ns_p50", percentiles[2].bucket().count(), i64),
            ("load_time_ns_p75", percentiles[3].bucket().count(), i64),
            ("load_time_ns_p90", percentiles[4].bucket().count(), i64),
            ("load_time_ns_p95", percentiles[5].bucket().count(), i64),
            ("load_time_ns_p99", percentiles[6].bucket().count(), i64),
            ("load_time_ns_p99.9", percentiles[7].bucket().count(), i64),
            ("load_time_ns_p99.99", percentiles[8].bucket().count(), i64),
            ("load_time_ns_p99.999", percentiles[9].bucket().count(), i64),
            ("load_time_ns_p100", percentiles[10].bucket().count(), i64),
        );

        let start = Instant::now();
        self.histogram.clear();
        error!(
            "bprumo DEBUG: LoadAccountsHistogram, time to clear: {:?}",
            start.elapsed()
        );
        *previous_submit = Instant::now();
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("datapoint_name", &self.datapoint_name)
            .field("submit_interval", &self.submit_interval)
            .field("previous_submit", &self.previous_submit.read().unwrap())
            .finish_non_exhaustive()
    }
}
