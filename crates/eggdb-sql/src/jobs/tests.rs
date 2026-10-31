use super::{CronSchedule, JobsRun, Minute, run_due_jobs};
use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

const MONDAY_2024_01_01_00_00: i64 = 1_704_067_200;

fn at(minutes_later: i64) -> i64 {
    MONDAY_2024_01_01_00_00 + minutes_later * 60
}

fn fires(schedule: &str, epoch: i64) -> bool {
    CronSchedule::parse(schedule).unwrap().fires_in(Minute::of_epoch(epoch))
}

#[test]
fn epoch_minutes_become_calendar_minutes() {
    assert_eq!(Minute::of_epoch(MONDAY_2024_01_01_00_00), Minute { minute: 0, hour: 0, day: 1, month: 1, weekday: 1 });
    assert_eq!(Minute::of_epoch(951_782_400 + 59 * 60 + 13 * 3_600), Minute { minute: 59, hour: 13, day: 29, month: 2, weekday: 2 });
    assert_eq!(Minute::of_epoch(-60), Minute { minute: 59, hour: 23, day: 31, month: 12, weekday: 3 });
}

#[test]
fn schedules_fire_in_the_minutes_cron_documents() {
    assert!(fires("* * * * *", at(17)));
    assert!(fires("*/15 * * * *", at(45)) && !fires("*/15 * * * *", at(46)));
    assert!(fires("30 9-17 * * mon-fri", at(9 * 60 + 30)));
    assert!(!fires("30 9-17 * * mon-fri", at(5 * 24 * 60 + 9 * 60 + 30)));
    assert!(fires("0 0 1 jan *", at(0)) && fires("@yearly", at(0)) && fires("@hourly", at(120)));
    assert!(fires("0 0 * * 7", at(6 * 24 * 60)), "7 is Sunday too");
    assert!(fires("0 0 15 * mon", at(0)), "either day field matches when both are restricted");
    assert!(!fires("0 0 15 * tue", at(0)));
    assert!(fires("5,10,20-22/2 * * * *", at(22)) && !fires("5,10,20-22/2 * * * *", at(21)));
}

#[test]
fn malformed_schedules_are_refused() {
    for schedule in ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "*/0 * * * *", "5-1 * * * *", "* * * foo *", "@sometimes", "a b c d e f"] {
        assert!(CronSchedule::parse(schedule).is_err(), "{schedule}");
    }
}

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session.execute(sql, &mut |result| out.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn pg_cron_jobs_run_when_due_and_record_their_runs() {
    let store = MemoryStore::default();
    assert_eq!(run_due_jobs(&store, at(0)).unwrap(), JobsRun::default());
    let mut session = Session::new(&store);
    run(&mut session, "CREATE EXTENSION pg_cron");
    run(&mut session, "CREATE EXTENSION IF NOT EXISTS pg_cron");
    assert!(session.execute("CREATE EXTENSION pg_cron", &mut |_| {}).is_err());
    run(&mut session, "CREATE TABLE ticks (at int)");
    run(&mut session, "SELECT cron.schedule('*/10 * * * *', 'INSERT INTO ticks VALUES (1)')");
    run(&mut session, "SELECT cron.schedule('nightly', '0 0 * * *', 'INSERT INTO missing VALUES (1)')");
    run(&mut session, "SELECT cron.schedule('nightly', '0 0 * * *', 'INSERT INTO ticks VALUES (2)')");
    run(&mut session, "SELECT cron.schedule('broken', 'not a schedule', 'SELECT 1')");
    assert_eq!(run_due_jobs(&store, at(0)).unwrap(), JobsRun { succeeded: 2, failed: 0, invalid_schedules: 1 });
    assert_eq!(run_due_jobs(&store, at(5)).unwrap().succeeded, 0);
    assert_eq!(run_due_jobs(&store, at(10)).unwrap().succeeded, 1);
    assert_eq!(run(&mut session, "SELECT sum(at) FROM ticks"), run(&mut session, "SELECT 4::bigint"));
    assert_eq!(run(&mut session, "SELECT count(*) FROM cron.job_run_details WHERE status = 'succeeded'"), run(&mut session, "SELECT 3::bigint"));
    run(&mut session, "SELECT cron.unschedule('nightly')");
    assert!(session.execute("SELECT cron.unschedule('nightly')", &mut |_| {}).is_err());
    run(&mut session, "DROP TABLE ticks");
    let failed = run_due_jobs(&store, at(20)).unwrap();
    assert_eq!(failed.failed, 1);
    assert_eq!(run(&mut session, "SELECT count(*) FROM cron.job_run_details WHERE status = 'failed'"), run(&mut session, "SELECT 1::bigint"));
}
