// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

#[test]
fn test_redate_sign_requires_signing_backend() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    let output = work_dir.run_jj(["redate", "--sign", "-r", "@"]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("No signing backend configured")
    );
}

#[test]
fn test_redate_retries_invalid_timestamp_and_can_cancel() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    let operation_id = work_dir.current_operation_id();

    let output = work_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_INTERACTIVE", "1")
                .env("TZ", "UTC")
                .args(["redate", "-r", "@"])
                .write_stdin("aaaaaa\n2002-03-04T05:06:07+00:00\nn\n")
        })
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Couldn't understand `aaaaaa` as a date and time"),
        "unexpected prompt output: {output}"
    );
    assert!(output.stderr.raw().contains("Timestamp [now]: "));
    assert!(
        output
            .stderr
            .raw()
            .contains("Hint: Enter a local date and time")
    );
    assert_eq!(work_dir.current_operation_id(), operation_id);
}

#[test]
fn test_redate_timestamp_argument_does_not_prompt() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    let output = work_dir
        .run_jj([
            "redate",
            "-r",
            "@",
            "--timestamp",
            "2002-03-04 05:06 -07:00",
        ])
        .success();
    assert!(!output.stderr.raw().contains("Timestamp"));
    assert!(!output.stderr.raw().contains("Redate 1 revision"));
    assert_eq!(
        commit_metadata(&work_dir, "@"),
        "2002-03-04 12:06:00|2002-03-04 12:06:00|unsigned\n"
    );
    assert_eq!(
        work_dir
            .run_jj([
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                r#"author.timestamp() ++ "\n""#,
            ])
            .success()
            .stdout
            .raw(),
        "2002-03-04 05:06:00.000 -07:00\n"
    );
}

#[test]
fn test_redate_local_timestamp_observes_daylight_saving_transitions() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    for (expression, expected) in [
        ("2024-03-10 03:30", "2024-03-10T03:30:00-04:00\n"),
        ("10 March 2024 3:30am", "2024-03-10T03:30:00-04:00\n"),
        ("2024-11-03 02:30", "2024-11-03T02:30:00-05:00\n"),
        ("2024-11-03T01:30:00-04:00", "2024-11-03T01:30:00-04:00\n"),
        ("2024-11-03T01:30:00-05:00", "2024-11-03T01:30:00-05:00\n"),
    ] {
        work_dir
            .run_jj_with(|cmd| {
                cmd.env("TZ", "America/New_York")
                    .args(["redate", "--timestamp", expression])
            })
            .success();
        assert_eq!(
            work_dir
                .run_jj([
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    r#"committer.timestamp().format("%Y-%m-%dT%H:%M:%S%:z") ++ "\n""#,
                ])
                .success()
                .stdout
                .raw(),
            expected,
            "wrong local offset for {expression}"
        );
    }

    for expression in ["2024-03-10 02:30", "10 March 2024 2:30am"] {
        let operation_id = work_dir.current_operation_id();
        let output = work_dir.run_jj_with(|cmd| {
            cmd.env("TZ", "America/New_York")
                .args(["redate", "--timestamp", expression])
        });
        assert!(
            !output.status.success(),
            "accepted nonexistent time: {expression}"
        );
        assert!(output.stderr.raw().contains("Couldn't understand"));
        assert_eq!(work_dir.current_operation_id(), operation_id);
    }
}

#[test]
fn test_redate_preserves_descendant_metadata_and_signs_on_request() {
    let test_env = TestEnvironment::default();
    test_env.add_config(
        r#"
        [signing]
        backend = "test"
        behavior = "keep"
        key = "impeccable"
        "#,
    );
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.run_jj(["describe", "-m", "target"]).success();
    work_dir.run_jj(["new", "-m", "signed child"]).success();
    work_dir.run_jj(["sign", "-r", "@"]).success();
    work_dir.run_jj(["new", "-m", "unsigned child"]).success();

    let expected_descendant_metadata = descendant_metadata(&work_dir);

    redate(&work_dir, "@--", "2002-03-04T05:06:07+00:00", false);
    assert_eq!(
        commit_metadata(&work_dir, "@--"),
        "2002-03-04 05:06:07|2002-03-04 05:06:07|unsigned\n"
    );
    assert_eq!(descendant_metadata(&work_dir), expected_descendant_metadata);

    let operation_id = work_dir.current_operation_id();
    redate(&work_dir, "@--", "2002-03-04T05:06:07+00:00", false);
    assert_eq!(work_dir.current_operation_id(), operation_id);

    redate(&work_dir, "@--", "2003-04-05T06:07:08+00:00", true);
    assert_eq!(
        commit_metadata(&work_dir, "@--"),
        "2003-04-05 06:07:08|2003-04-05 06:07:08|good\n"
    );
    assert_eq!(descendant_metadata(&work_dir), expected_descendant_metadata);
}

fn redate(work_dir: &TestWorkDir, revision: &str, timestamp: &str, sign: bool) {
    work_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_INTERACTIVE", "1").env("TZ", "UTC");
            cmd.arg("redate");
            if sign {
                cmd.arg("--sign");
            }
            cmd.args(["-r", revision])
                .write_stdin(format!("{timestamp}\ny\n"))
        })
        .success();
}

fn commit_metadata(work_dir: &TestWorkDir, revision: &str) -> String {
    work_dir
        .run_jj([
            "log",
            "--no-graph",
            "-r",
            revision,
            "-T",
            r#"author.timestamp().utc().format("%Y-%m-%d %H:%M:%S")
              ++ "|" ++ committer.timestamp().utc().format("%Y-%m-%d %H:%M:%S")
              ++ "|" ++ if(signature, signature.status(), "unsigned") ++ "\n""#,
        ])
        .success()
        .stdout
        .raw()
        .to_owned()
}

fn descendant_metadata(work_dir: &TestWorkDir) -> String {
    work_dir
        .run_jj([
            "log",
            "--no-graph",
            "-r",
            "@|@-",
            "-T",
            r#"change_id ++ "|" ++ author ++ "|" ++ committer
              ++ "|" ++ if(signature, signature.status(), "unsigned") ++ "\n""#,
        ])
        .success()
        .stdout
        .raw()
        .to_owned()
}
