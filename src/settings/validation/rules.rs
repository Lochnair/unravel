// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::time::{Duration, Instant};

pub(crate) fn nonblank(value: &str, _: &()) -> garde::Result {
    if value.trim().is_empty() {
        return Err(garde::Error::new("must not be blank"));
    }
    Ok(())
}

fn nonzero_duration(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|period| !period.is_zero())
}

pub(crate) fn change_interval(value: &f64, _: &()) -> garde::Result {
    if nonzero_duration(*value).is_none() {
        return Err(garde::Error::new(
            "must be a nonzero representable duration",
        ));
    }
    Ok(())
}

pub(crate) fn timer_period(value: &f64, _: &()) -> garde::Result {
    if !nonzero_duration(*value).is_some_and(|period| Instant::now().checked_add(period).is_some())
    {
        return Err(garde::Error::new("must be a schedulable nonzero duration"));
    }
    Ok(())
}
