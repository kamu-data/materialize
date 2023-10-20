// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.

//! Report rusage metrics.

use std::ops::Add;
use std::time::Duration;

use mz_ore::metrics::MetricsRegistry;
use prometheus::{Gauge, IntGauge};

macro_rules! metrics {
    ($namespace:ident $(($name:ident, $desc:expr, $suffix:expr, $type:ident)),*) => {
        metrics! { @define $namespace $(($name, $desc, $suffix, $type)),*}
    };
    (@define $namespace:ident $(($name:ident, $desc:expr, $suffix:expr, $type:ident)),*) => {
        struct RuMetrics {
            $($name: <$type as Unit>::Gauge,)*
        }
        impl RuMetrics {
            fn new(registry: &MetricsRegistry) -> Self {
                Self {
                    $($name: registry.register(mz_ore::metric!(
                        name: concat!(stringify!($namespace), "_", stringify!($name), $suffix),
                        help: $desc,
                    )),)*
                }
            }
            fn update(&self) {
                let rusage = unsafe {
                    let mut rusage = std::mem::zeroed();
                    let ret = libc::getrusage(libc::RUSAGE_SELF, &mut rusage);
                    if ret < 0 {
                        return;
                    }
                    rusage
                };
                $(self.$name.set(<$type as Unit>::from(rusage.$name));)*
            }
        }
    };
}

/// Type for converting values from POSIX to Prometheus.
trait Unit {
    /// Prometheus gauge
    type Gauge;
    /// Libc type
    type From;
    /// Gauge type.
    type To;
    /// Convert an actual value.
    fn from(value: Self::From) -> Self::To;
}

/// Unit for converting POSIX timeval.
struct Timeval;
impl Unit for Timeval {
    type Gauge = Gauge;
    type From = libc::timeval;
    type To = f64;
    fn from(Self::From { tv_sec, tv_usec }: Self::From) -> Self::To {
        // timeval can capture negative values; it'd be surprising to see a negative values here.
        Duration::from_secs(tv_sec.abs_diff(0))
            .add(Duration::from_micros(tv_usec.abs_diff(0)))
            .as_secs_f64()
    }
}

/// Unit for direct conversion to i64.
struct Unitless;
impl Unit for Unitless {
    type Gauge = IntGauge;
    type From = libc::c_long;
    type To = i64;
    fn from(value: Self::From) -> Self::To {
        value
    }
}

metrics! {
    mz_metrics_libc
    (ru_utime, "user CPU time used", "_s", Timeval),
    (ru_stime, "system CPU time used", "_s", Timeval),
    (ru_maxrss, "maximum resident set size", "", Unitless),
    (ru_ixrss, "integral shared memory size", "", Unitless),
    (ru_idrss, "integral unshared data size", "", Unitless),
    (ru_isrss, "integral unshared stack size", "", Unitless),
    (ru_minflt, "page reclaims (soft page faults)", "", Unitless),
    (ru_majflt, "page faults (hard page faults)", "", Unitless),
    (ru_nswap, "swaps", "", Unitless),
    (ru_inblock, "block input operations", "", Unitless),
    (ru_oublock, "block output operations", "", Unitless),
    (ru_msgsnd, "IPC messages sent", "", Unitless),
    (ru_msgrcv, "IPC messages received", "", Unitless),
    (ru_nsignals, "signals received", "", Unitless),
    (ru_nvcsw, "voluntary context switches", "", Unitless),
    (ru_nivcsw, "involuntary context switches", "", Unitless)
}

/// Register a task to read rusage stats.
#[allow(clippy::unused_async)]
pub async fn register_metrics_into(metrics_registry: &MetricsRegistry) {
    let rusage = RuMetrics::new(metrics_registry);

    mz_ore::task::spawn(|| "rusage_stats_update", async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            rusage.update();
        }
    });
}
