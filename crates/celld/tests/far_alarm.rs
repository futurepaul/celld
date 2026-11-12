//! An object's alarm years ahead, as workerd takes it: stored with its time,
//! and the node that arms it stays up, at once and after a restart.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use std::time::{Duration, Instant};

use support::{project, Dev};

/// 2100-01-01T00:00:00Z: past the timer queue's span (about 2.2 years).
const FAR_MS: u64 = 4_102_444_800_000;

const WORKER: &str = r#"
export class Clock {
  constructor(ctx) {
    this.ctx = ctx;
  }

  async fetch(request) {
    const url = new URL(request.url);
    if (url.searchParams.has("set")) {
      await this.ctx.storage.setAlarm(Number(url.searchParams.get("set")));
    }
    if (url.searchParams.has("in")) {
      await this.ctx.storage.setAlarm(Date.now() + Number(url.searchParams.get("in")));
    }
    return Response.json({
      alarm: await this.ctx.storage.getAlarm(),
      fired: (await this.ctx.storage.get("fired")) ?? 0,
    });
  }

  async alarm() {
    const fired = (await this.ctx.storage.get("fired")) ?? 0;
    await this.ctx.storage.put("fired", fired + 1);
  }
}

export default {
  fetch(request, env) {
    const name = new URL(request.url).searchParams.get("name");
    return env.CLOCK.getByName(name).fetch(request);
  },
};
"#;

/// The node is still serving a while after `why`, its process not exited.
fn stays_up(dev: &mut Dev, why: &str) {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(status) = dev.exited() {
            panic!("celld dev exited with {status} {why}:\n{}", dev.output());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (status, _) = dev
        .get("/.well-known/celld/health")
        .unwrap_or_else(|error| panic!("health {why}: {error}\n{}", dev.output()));
    assert_eq!(status, 200, "health {why}:\n{}", dev.output());
}

#[test]
fn an_alarm_years_ahead_is_kept_and_the_node_stays_up() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), WORKER, &[("CLOCK", "Clock")]);

    let mut dev = Dev::start(dir.path());
    let (seen, _) = dev.json(&format!("/?name=far&set={FAR_MS}"));
    assert_eq!(seen, serde_json::json!({ "alarm": FAR_MS, "fired": 0 }));
    stays_up(&mut dev, "after the far alarm was set");

    // An alarm due now still fires once, on time, beside the far one.
    let (seen, _) = dev.json("/?name=near&in=500");
    assert_eq!(seen["fired"], 0);
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let (seen, _) = dev.json("/?name=near");
        if seen == serde_json::json!({ "alarm": null, "fired": 1 }) {
            break;
        }
        assert!(Instant::now() < until, "the near alarm: {seen}\n{}", dev.output());
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(dev);

    // The node arms its stored alarms at start: again with the far one.
    let mut dev = Dev::start(dir.path());
    stays_up(&mut dev, "after a restart with the far alarm stored");
    let (seen, _) = dev.json("/?name=far");
    assert_eq!(seen, serde_json::json!({ "alarm": FAR_MS, "fired": 0 }));
    stays_up(&mut dev, "after the far alarm's object woke");
}
